//! SQL's joins, run: each answer held to the rows SQL answers, worked out
//! here by nested loops over each table's own rows -- inner and LEFT, to
//! one row and to many, chained, filtered on either table, ordered, paged
//! -- and to the FenecQL `lookup` written by hand, flattened.

use fenec_core::prelude::*;
use fenec_ql::{parse, parse_one};
use std::collections::BTreeMap;

fn run(db: &mut Database, q: &str) {
    for s in parse(q).unwrap_or_else(|e| panic!("{q}: {e}")) {
        db.execute_with(&s, &[])
            .unwrap_or_else(|e| panic!("{q}: {e}"));
    }
}

/// A statement's answer as a transport writes it: SQL's flat rows.
fn answer(db: &Database, q: &str) -> (Vec<String>, Vec<Vec<Value>>) {
    let st = parse_one(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    let r = db.query(&st, &[]).unwrap_or_else(|e| panic!("{q}: {e}"));
    let rs = r.rows().unwrap_or_else(|| panic!("{q}: no rows"));
    let rs = rs.answered();
    (
        rs.columns.clone(),
        rs.rows.iter().map(|r| r.values.clone()).collect(),
    )
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

/// Customers, their orders and the orders' lines: an order's customer may
/// be missing or none, a customer and an order may have no children.
fn shop(rng: &mut Rng) -> Database {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection c (name text, country text @hash, rank int); \
         create collection o (cid int @hash, total int, status text); \
         create collection l (oid int @hash, qty int)",
    );
    let countries = ["IT", "DE", "TR"];
    for i in 0..24 {
        let rank = match rng.below(5) {
            0 => "null".to_string(),
            r => r.to_string(),
        };
        run(
            &mut db,
            &format!(
                "put c {{name: 'n{}', country: '{}', rank: {rank}}}",
                i % 9,
                countries[rng.below(3) as usize]
            ),
        );
    }
    for _ in 0..70 {
        let cid = match rng.below(9) {
            0 => "null".to_string(),
            _ => (1 + rng.below(30)).to_string(),
        };
        let status = ["paid", "open", "cancelled"][rng.below(3) as usize];
        run(
            &mut db,
            &format!(
                "put o {{cid: {cid}, total: {}, status: '{status}'}}",
                rng.below(50)
            ),
        );
    }
    for _ in 0..160 {
        run(
            &mut db,
            &format!(
                "put l {{oid: {}, qty: {}}}",
                1 + rng.below(80),
                rng.below(6)
            ),
        );
    }
    db
}

/// A table's rows by id, every field.
fn table(db: &Database, name: &str) -> BTreeMap<i64, BTreeMap<String, Value>> {
    let st = parse_one(&format!("get {name}")).unwrap();
    let r = db.query(&st, &[]).unwrap();
    let rs = r.rows().unwrap();
    let mut out = BTreeMap::new();
    for row in &rs.rows {
        let mut m = BTreeMap::new();
        for (c, v) in rs.columns.iter().zip(&row.values) {
            m.insert(c.clone(), v.clone());
        }
        let Value::Int(id) = m["id"] else { panic!() };
        out.insert(id, m);
    }
    out
}

/// The ids of a table's rows a condition holds for, as FenecQL reads it.
fn passing(db: &Database, name: &str, cond: &str) -> Vec<i64> {
    let q = match cond {
        "" => format!("get {name} select id"),
        c => format!("get {name} select id where {c}"),
    };
    let r = db.query(&parse_one(&q).unwrap(), &[]).unwrap();
    r.rows()
        .unwrap()
        .rows
        .iter()
        .map(|r| match r.values[0] {
            Value::Int(i) => i,
            _ => panic!(),
        })
        .collect()
}

/// One table of a join as the generator writes it: its collection, alias,
/// the key it is found by and the key of the table before it, a condition
/// on its own fields, and which of its fields the list names.
struct Side {
    name: &'static str,
    alias: &'static str,
    fields: &'static [&'static str],
}

const C: Side = Side {
    name: "c",
    alias: "cu",
    fields: &["name", "country", "rank"],
};
const O: Side = Side {
    name: "o",
    alias: "od",
    fields: &["cid", "total", "status"],
};
const L: Side = Side {
    name: "l",
    alias: "li",
    fields: &["oid", "qty"],
};

fn conds(side: &Side) -> &'static [&'static str] {
    match side.name {
        "c" => &["country = 'IT'", "rank > 2", "name like 'n1%'", ""],
        "o" => &["status = 'paid'", "total < 25", "status != 'open'", ""],
        _ => &["qty > 2", "qty = 0", ""],
    }
}

/// Over generated shops and joins, every answer is SQL's: the rows of the
/// pairs -- or paths, chained -- that match, the first table's rows no
/// match was found for kept with `null`s by a LEFT JOIN, the list's
/// columns under SQL's names, in the order asked and paged.
#[test]
fn a_join_answers_the_rows_sql_does() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let (mut checked, mut ways) = (0, [0; 3]);
    for _ in 0..6 {
        let db = shop(&mut rng);
        let tabs: BTreeMap<&str, _> = ["c", "o", "l"]
            .into_iter()
            .map(|n| (n, table(&db, n)))
            .collect();
        for _ in 0..120 {
            // The chain and its keys: to one row (by `id`) or to many.
            let shape = rng.below(4);
            let (sides, keys): (Vec<&Side>, Vec<(&str, &str)>) = match shape {
                // an order's customer: o -> c by c.id = o.cid
                0 => (vec![&O, &C], vec![("id", "cid")]),
                // a customer's orders: c -> o by o.cid = c.id
                1 => (vec![&C, &O], vec![("cid", "id")]),
                // an order's lines, then each line nothing more
                2 => (vec![&O, &L], vec![("oid", "id")]),
                // a line's order, and that order's customer
                _ => (vec![&L, &O, &C], vec![("id", "oid"), ("id", "cid")]),
            };
            let left = rng.below(2) == 1;
            let n = sides.len();
            // The list: some fields of each table, named apart -- or of
            // the first alone, of the second alone, or a count -- which
            // reach each reading: a `lookup` flattened, the other table a
            // filter, the joined table's rows with the first's a filter.
            let mode = match n {
                2 => rng.below(4),
                _ => rng.below(2),
            };
            let mode = if mode == 3 && left && shape != 0 {
                0
            } else {
                mode
            };
            let mut list: Vec<(usize, &str, String)> = Vec::new();
            for (t, s) in sides.iter().enumerate() {
                let wanted = match mode {
                    0 => true,
                    1 => t == 0,
                    2 => t == 1,
                    _ => false,
                };
                for f in s.fields.iter().chain(&["id"]).filter(|_| wanted) {
                    let last = if mode == 0 { n - 1 } else { t };
                    if rng.below(3) == 0 || (t == last && list.iter().all(|x| x.0 != t)) {
                        list.push((t, f, format!("{}_{f}", s.name)));
                    }
                }
            }
            if rng.below(4) == 0 {
                list.reverse();
            }
            let counted = mode == 3;
            let distinct = n == 2 && (mode == 1 || (mode == 2 && !left)) && rng.below(3) == 0;
            // Conditions: the first table's in the `where`; the others'
            // in the `on` of a LEFT JOIN, in the `where` of an inner.
            let cond: Vec<&str> = sides
                .iter()
                .map(|s| conds(s)[rng.below(conds(s).len() as u64) as usize])
                .collect();
            let qualified = |t: usize, c: &str| -> String {
                if c.is_empty() {
                    return String::new();
                }
                let s = sides[t];
                let field = c.split_whitespace().next().unwrap();
                c.replacen(field, &format!("{}.{field}", s.alias), 1)
            };
            let mut sql = String::from(if distinct {
                "SELECT DISTINCT "
            } else {
                "SELECT "
            });
            let cols: Vec<String> = list
                .iter()
                .map(|(t, f, as_)| format!("{}.{f} AS {as_}", sides[*t].alias))
                .collect();
            sql.push_str(&cols.join(", "));
            if counted {
                sql.push_str("count(*) AS n");
            }
            sql.push_str(&format!(" FROM {} {}", sides[0].name, sides[0].alias));
            let mut wheres = Vec::new();
            if !cond[0].is_empty() {
                wheres.push(qualified(0, cond[0]));
            }
            for j in 1..n {
                let (child, parent) = keys[j - 1];
                sql.push_str(&format!(
                    " {} {} {} ON {}.{child} = {}.{parent}",
                    if left { "LEFT JOIN" } else { "JOIN" },
                    sides[j].name,
                    sides[j].alias,
                    sides[j].alias,
                    sides[j - 1].alias
                ));
                if !cond[j].is_empty() {
                    match left {
                        true => sql.push_str(&format!(" AND {}", qualified(j, cond[j]))),
                        false => wheres.push(qualified(j, cond[j])),
                    }
                }
            }
            if !wheres.is_empty() {
                sql.push_str(&format!(" WHERE {}", wheres.join(" AND ")));
            }
            let ordered = rng.below(3) != 0 && !distinct && !counted;
            let key = sides[0].fields[rng.below(sides[0].fields.len() as u64) as usize];
            let desc = rng.below(2) == 1;
            if ordered {
                sql.push_str(&format!(
                    " ORDER BY {}.{key}{}, {}.id",
                    sides[0].alias,
                    if desc { " DESC" } else { "" },
                    sides[0].alias
                ));
            }
            let page = ordered && rng.below(2) == 0;
            let (limit, offset) = (rng.below(12) as usize, rng.below(6) as usize);
            if page {
                sql.push_str(&format!(" LIMIT {limit} OFFSET {offset}"));
            }

            // SQL's answer by nested loops: each path down the chain.
            let keep: Vec<Vec<i64>> = (0..n)
                .map(|t| passing(&db, sides[t].name, cond[t]))
                .collect();
            let mut paths: Vec<Vec<Option<i64>>> =
                keep[0].iter().map(|&id| vec![Some(id)]).collect();
            for j in 1..n {
                let (child, parent) = keys[j - 1];
                let mut next = Vec::new();
                for p in paths {
                    let up = p[j - 1].map(|id| tabs[sides[j - 1].name][&id][parent].clone());
                    let found: Vec<i64> = match &up {
                        Some(v) if *v != Value::Null => keep[j]
                            .iter()
                            .copied()
                            .filter(|id| tabs[sides[j].name][id][child] == *v)
                            .collect(),
                        _ => Vec::new(),
                    };
                    if found.is_empty() {
                        if left {
                            let mut q = p.clone();
                            q.push(None);
                            next.push(q);
                        }
                        continue;
                    }
                    for id in found {
                        let mut q = p.clone();
                        q.push(Some(id));
                        next.push(q);
                    }
                }
                paths = next;
            }
            let value = |p: &Vec<Option<i64>>, t: usize, f: &str| match p[t] {
                Some(id) => tabs[sides[t].name][&id][f].clone(),
                None => Value::Null,
            };
            if ordered {
                paths.sort_by(|a, b| {
                    let k = value(a, 0, key).cmp_value(&value(b, 0, key));
                    let k = if desc { k.reverse() } else { k };
                    k.then(a.cmp(b))
                });
            }
            let mut want: Vec<Vec<Value>> = paths
                .iter()
                .map(|p| list.iter().map(|(t, f, _)| value(p, *t, f)).collect())
                .collect();
            if page {
                want = want.into_iter().skip(offset).take(limit).collect();
            }
            if distinct {
                want.sort_by_key(|r| fenec_core::json::to_string(&Value::List(r.clone())));
                want.dedup();
            }
            if counted {
                want = vec![vec![Value::Int(paths.len() as i64)]];
            }
            let (columns, mut got) = answer(&db, &sql);
            let mut names: Vec<String> = list.iter().map(|x| x.2.clone()).collect();
            if counted {
                names.push("n".into());
            }
            assert_eq!(columns, names, "{sql}");
            if !ordered {
                let key = |r: &Vec<Value>| fenec_core::json::to_string(&Value::List(r.clone()));
                got.sort_by_key(key);
                want.sort_by_key(key);
            }
            assert_eq!(got, want, "{sql}");
            checked += 1;
            // Which reading answered.
            let Statement::Select(s) = parse_one(&sql).unwrap() else {
                panic!()
            };
            let at = match (&s.flat, s.filter.as_ref().is_some_and(|f| f.has_subquery())) {
                (Some(_), _) => 0,
                (None, true) => 1,
                (None, false) => 2,
            };
            ways[at] += 1;
        }
    }
    assert_eq!(checked, 720);
    // Flat rows, a filter of the other table's, and the join dropped.
    assert!(ways.iter().all(|&n| n > 40), "{ways:?}");
}

/// The join is the `lookup` it reads as, answered flat: the hand-written
/// FenecQL's rows flattened, a parent with no child a row of `null`s
/// under a LEFT JOIN, and no row under an inner one.
#[test]
fn a_join_is_its_lookup_flattened() {
    let mut db = shop(&mut Rng(7));
    run(
        &mut db,
        "put c {name: 'alone', country: 'TR', rank: 1}; del o where cid in (get c select id \
         where name = 'alone')",
    );
    for (sql, fenecql) in [
        (
            "SELECT cu.name, od.total FROM c cu LEFT JOIN o od ON od.cid = cu.id ORDER BY cu.id",
            "get c select name order id lookup o on cid select total",
        ),
        (
            "SELECT cu.name, od.total FROM c cu JOIN o od ON od.cid = cu.id ORDER BY cu.id",
            "get c select name order id lookup o on cid required select total",
        ),
        (
            "SELECT li.qty, od.status, cu.name FROM l li JOIN o od ON od.id = li.oid LEFT JOIN c \
             cu ON cu.id = od.cid WHERE li.qty > 1 ORDER BY li.id",
            "get l select qty where qty > 1 order id lookup o on id = oid required select status \
             lookup c on id = cid select name",
        ),
    ] {
        let (_, got) = answer(&db, sql);
        let st = parse_one(fenecql).unwrap();
        let r = db.query(&st, &[]).unwrap();
        let flat = r.rows().unwrap().flatten();
        let want: Vec<Vec<Value>> = flat.rows.iter().map(|r| r.values.clone()).collect();
        assert_eq!(got, want, "{sql}");
        assert!(!got.is_empty(), "{sql}");
    }
    // The LEFT JOIN keeps the customer with no order, `null` beside it.
    let (_, rows) = answer(
        &db,
        "SELECT cu.name, od.id FROM c cu LEFT JOIN o od ON od.cid = cu.id WHERE cu.name = 'alone'",
    );
    assert_eq!(rows, [vec![Value::Text("alone".into()), Value::Null]]);
    let (_, rows) = answer(
        &db,
        "SELECT cu.name, od.id FROM c cu JOIN o od ON od.cid = cu.id WHERE cu.name = 'alone'",
    );
    assert!(rows.is_empty());
    // SQL's names: a column under its field's name or its `as`, `*` each
    // field's -- and two tables' `id`s refused rather than one dropped.
    let (cols, _) = answer(
        &db,
        "SELECT od.id, cu.name FROM o od JOIN c cu ON cu.id = od.cid",
    );
    assert_eq!(cols, ["id", "name"]);
    let (cols, _) = answer(
        &db,
        "SELECT od.*, cu.name AS who FROM o od JOIN c cu ON cu.id = od.cid",
    );
    assert_eq!(cols, ["id", "cid", "total", "status", "who"]);
    for q in [
        "SELECT * FROM o od JOIN c cu ON cu.id = od.cid",
        "SELECT od.id, cu.id FROM o od JOIN c cu ON cu.id = od.cid",
    ] {
        let e = db
            .query(&parse_one(q).unwrap(), &[])
            .unwrap_err()
            .to_string();
        assert!(e.contains("two columns named `id`"), "{q}: {e}");
    }
    // A join's key the joined table is read by needs its index, as a
    // `lookup`'s does.
    let e = db
        .query(
            &parse_one("SELECT cu.name, od.total FROM c cu JOIN o od ON od.total = cu.rank")
                .unwrap(),
            &[],
        )
        .unwrap_err()
        .to_string();
    assert!(e.contains("needs a hash index"), "{e}");
    // A page with a parameter, cut from the flat rows.
    let st = parse_one(
        "SELECT cu.name, od.total FROM c cu JOIN o od ON od.cid = cu.id ORDER BY cu.id LIMIT $1 \
         OFFSET $2",
    )
    .unwrap();
    let all = answer(
        &db,
        "SELECT cu.name, od.total FROM c cu JOIN o od ON od.cid = cu.id ORDER BY cu.id",
    )
    .1;
    let r = db.query(&st, &[Value::Int(4), Value::Int(3)]).unwrap();
    let got: Vec<Vec<Value>> = (r.rows().unwrap().answered().rows.iter())
        .map(|r| r.values.clone())
        .collect();
    assert_eq!(got, all[3..7]);
    assert_eq!(st.max_param(), 2);
}

/// A JSON writer's answer is the flat rows, and a FenecQL `lookup`'s still
/// nests its children.
#[test]
fn a_join_is_written_flat_and_a_lookup_nested() {
    let db = shop(&mut Rng(11));
    let json = |q: &str| {
        let r = db.query(&parse_one(q).unwrap(), &[]).unwrap();
        let mut out = String::new();
        fenec_core::json::rows_array_into(&mut out, r.rows().unwrap());
        out
    };
    let flat =
        json("SELECT od.id, cu.name FROM o od JOIN c cu ON cu.id = od.cid ORDER BY od.id LIMIT 1");
    assert!(
        flat.starts_with("[{\"id\":") && flat.contains("\"name\":"),
        "{flat}"
    );
    assert!(!flat.contains("\"c\":["), "{flat}");
    let nested = json("get o select id order id limit 1 lookup c on id = cid select name");
    assert!(nested.contains("\"c\":["), "{nested}");
}
