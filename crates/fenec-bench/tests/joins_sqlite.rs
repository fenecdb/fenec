//! SQL's joins, `EXISTS`, `DISTINCT` and aggregates over joins, each held to
//! what SQLite answers for the same rows: SQL's three-valued logic worked
//! out by a SQL engine, not by FenecQL -- `crates/fenec-ql/tests/join.rs`
//! reads each table's conditions through FenecQL, so a reading that took
//! a null otherwise than SQL could not fail there. A statement fenecdb
//! refuses passes; one it answers must answer SQLite's rows, and so must
//! the probe a join read as a list may be read as; how many of each is pinned,
//! and CI runs it on the clippy job. The conditions
//! keep clear of what FenecQL documents reading otherwise (`!=` and `NOT
//! IN` beside a null, sql.html#nulls). Here and not in fenec-ql, since
//! SQLite is this crate's alone: `cargo test -p fenec-bench`.

use fenec_core::prelude::*;
use rusqlite::types::ValueRef;

struct Rng(u64);
impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// Customers whose names repeat, their orders (a customer missing or
/// none), the orders' lines, and events keyed by a customer's name -- a
/// key many rows of both sides share.
const SCHEMA: [(&str, &str); 4] = [
    (
        "create collection c (name text @hash, country text @hash, rank int)",
        "create table c (id integer primary key, name text, country text, rank integer)",
    ),
    (
        "create collection o (cid int @hash, total int, status text)",
        "create table o (id integer primary key, cid integer, total integer, status text)",
    ),
    (
        "create collection l (oid int @hash, qty int)",
        "create table l (id integer primary key, oid integer, qty integer)",
    ),
    (
        "create collection e (name text @hash, amt int)",
        "create table e (id integer primary key, name text, amt integer)",
    ),
];

fn int_or_null(v: Option<u64>) -> String {
    v.map_or("null".into(), |v| v.to_string())
}

/// The same rows in both, ids made by fenecdb in order and written to
/// SQLite.
fn load(rng: &mut Rng) -> (Database, rusqlite::Connection) {
    let mut db = Database::new();
    let lite = rusqlite::Connection::open_in_memory().unwrap();
    for (f, s) in SCHEMA {
        db.execute(&fenec_ql::parse_one(f).unwrap()).unwrap();
        lite.execute(s, []).unwrap();
    }
    let mut both = |table: &str, cols: &[&str], vals: Vec<String>| {
        let doc: Vec<String> = cols
            .iter()
            .zip(&vals)
            .map(|(c, v)| format!("{c}: {v}"))
            .collect();
        db.execute(&fenec_ql::parse_one(&format!("put {table} {{{}}}", doc.join(", "))).unwrap())
            .unwrap();
        let vals: Vec<String> = vals.iter().map(|v| v.replace('"', "'")).collect();
        lite.execute(
            &format!(
                "insert into {table} ({}) values ({})",
                cols.join(", "),
                vals.join(", ")
            ),
            [],
        )
        .unwrap();
    };
    let countries = ["\"IT\"", "\"DE\"", "\"TR\"", "null"];
    for i in 0..30 {
        let rank = match rng.below(4) {
            0 => None,
            r => Some(r),
        };
        both(
            "c",
            &["name", "country", "rank"],
            vec![
                format!("\"n{}\"", i % 11),
                countries[rng.below(4) as usize].into(),
                int_or_null(rank),
            ],
        );
    }
    for _ in 0..80 {
        let cid = match rng.below(8) {
            0 => None,
            _ => Some(1 + rng.below(34)),
        };
        let status = ["\"paid\"", "\"open\"", "null"][rng.below(3) as usize];
        both(
            "o",
            &["cid", "total", "status"],
            vec![int_or_null(cid), rng.below(60).to_string(), status.into()],
        );
    }
    for _ in 0..150 {
        both(
            "l",
            &["oid", "qty"],
            vec![
                (1 + rng.below(90)).to_string(),
                int_or_null(Some(rng.below(6))),
            ],
        );
    }
    for _ in 0..40 {
        let name = match rng.below(9) {
            0 => "null".to_string(),
            k => format!("\"n{}\"", k + 3),
        };
        both("e", &["name", "amt"], vec![name, rng.below(20).to_string()]);
    }
    (db, lite)
}

fn sqlite_rows(lite: &rusqlite::Connection, sql: &str) -> Vec<Vec<Value>> {
    let mut st = lite
        .prepare(sql)
        .unwrap_or_else(|e| panic!("sqlite: {sql}: {e}"));
    let n = st.column_count();
    let mut rows = st.query([]).unwrap();
    let mut out = Vec::new();
    while let Some(r) = rows.next().unwrap() {
        out.push(
            (0..n)
                .map(|i| match r.get_ref(i).unwrap() {
                    ValueRef::Null => Value::Null,
                    ValueRef::Integer(v) => Value::Int(v),
                    ValueRef::Real(v) => Value::Float(v),
                    ValueRef::Text(t) => Value::Text(String::from_utf8_lossy(t).into()),
                    ValueRef::Blob(_) => panic!("{sql}: a blob"),
                })
                .collect(),
        );
    }
    out
}

/// fenecdb's flat rows, or the refusal.
fn fenec_rows(db: &Database, sql: &str) -> std::result::Result<Vec<Vec<Value>>, String> {
    let st = fenec_ql::parse_one(sql).map_err(|e| e.to_string())?;
    statement_rows(db, &st)
}

/// The flat rows `st` answers, or the refusal.
fn statement_rows(db: &Database, st: &Statement) -> std::result::Result<Vec<Vec<Value>>, String> {
    let r = db.query(st, &[]).map_err(|e| e.to_string())?;
    let rs = r.rows().ok_or_else(|| "no rows".to_string())?;
    let rs = rs.answered();
    Ok(rs.rows.iter().map(|r| r.values.clone()).collect())
}

fn sorted(mut rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    let key = |r: &Vec<Value>| fenec_core::json::to_string(&Value::List(r.clone()));
    rows.sort_by_key(key);
    rows
}

/// The statements: each shape of join, `EXISTS` and `NOT EXISTS`, names
/// with no table, `DISTINCT` and aggregates over pairs, with conditions on
/// either side drawn. Each written alike, a few with nothing drawn in.
#[allow(clippy::useless_format)]
fn statements(rng: &mut Rng) -> Vec<String> {
    let c_cond = [
        "cu.country = 'IT'",
        "cu.rank > 1",
        "cu.rank IS NULL",
        "cu.name LIKE 'n1%'",
    ];
    let o_cond = [
        "od.status = 'paid'",
        "od.total < 30",
        "od.status IS NULL",
        "od.cid IS NOT NULL",
    ];
    let pick = |rng: &mut Rng, list: &[&'static str]| list[rng.below(list.len() as u64) as usize];
    let mut out: Vec<String> = Vec::new();
    for _ in 0..12 {
        let (cc, oc) = (pick(rng, &c_cond), pick(rng, &o_cond));
        out.extend([
            format!("SELECT cu.id, od.id AS oid, od.total FROM c cu JOIN o od ON od.cid = cu.id WHERE {cc}"),
            format!("SELECT cu.id, od.total FROM c cu LEFT JOIN o od ON od.cid = cu.id AND {oc}"),
            format!("SELECT od.id, cu.name FROM o od JOIN c cu ON cu.id = od.cid WHERE {oc} AND {cc}"),
            format!("SELECT od.id, cu.rank FROM o od LEFT JOIN c cu ON cu.id = od.cid AND {cc}"),
            format!("SELECT cu.id FROM c cu LEFT JOIN o od ON od.cid = cu.id WHERE od.id IS NULL"),
            format!("SELECT cu.name FROM c cu LEFT JOIN o od ON od.cid = cu.id AND {oc} WHERE od.id IS NULL"),
            format!("SELECT id, name FROM c cu WHERE EXISTS (SELECT 1 FROM o od WHERE od.cid = cu.id AND {oc})"),
            format!("SELECT id FROM c cu WHERE NOT EXISTS (SELECT 1 FROM o od WHERE od.cid = cu.id AND {oc})"),
            format!("SELECT id FROM o od WHERE NOT EXISTS (SELECT 1 FROM c cu WHERE cu.id = od.cid AND {cc})"),
            format!("SELECT DISTINCT cu.name FROM c cu JOIN o od ON od.cid = cu.id WHERE {oc}"),
            format!("SELECT DISTINCT od.status FROM o od JOIN c cu ON cu.id = od.cid WHERE {cc}"),
            format!("SELECT count(*), sum(od.total), max(od.total) FROM o od JOIN c cu ON cu.id = od.cid WHERE {cc}"),
            format!("SELECT DISTINCT count(*) FROM c cu JOIN e ev ON ev.name = cu.name WHERE {cc}"),
            format!("SELECT sum(ev.amt), count(*) FROM c cu JOIN e ev ON ev.name = cu.name"),
            format!("SELECT count(*), max(cu.rank) FROM c cu JOIN o od ON od.cid = cu.id WHERE {oc}"),
            format!("SELECT cu.id, ev.amt FROM c cu JOIN e ev ON ev.name = cu.name WHERE {cc}"),
            format!("SELECT cu.id, ev.amt FROM c cu LEFT JOIN e ev ON ev.name = cu.name"),
            format!("SELECT total FROM o od JOIN c cu ON cu.id = od.cid WHERE country = 'IT'"),
            format!("SELECT li.id, li.qty, cu.name FROM l li JOIN o od ON od.id = li.oid JOIN c cu ON cu.id = od.cid WHERE {cc}"),
            format!("SELECT DISTINCT count(*) FROM o od JOIN c cu ON cu.id = od.cid WHERE {oc}"),
            format!("SELECT li.id, od.status FROM l li JOIN o od ON od.id = li.oid LEFT JOIN c cu ON cu.id = od.cid AND {cc}"),
            format!("SELECT od.id FROM o od, c cu WHERE cu.id = od.cid AND {cc}"),
            format!("SELECT cu.country, count(*) FROM c cu JOIN o od ON od.cid = cu.id GROUP BY cu.country"),
            // The joined table's rows, the first a filter, and the joined
            // table found by its `id` and filtered: each read as a list,
            // its probe beside it.
            format!("SELECT od.id, od.total FROM c cu JOIN o od ON od.cid = cu.id WHERE {cc} AND {oc}"),
            format!("SELECT od.id, od.status FROM o od JOIN c cu ON cu.id = od.cid WHERE {cc}"),
            format!("SELECT li.qty FROM l li JOIN o od ON od.id = li.oid WHERE {oc}"),
        ]);
    }
    out
}

/// What the statements come to: answered as SQLite answers, refused, and
/// read as a list with a probe beside it.
const ANSWERED: usize = 1008;
const REFUSED: usize = 240;
const PROBED: usize = 240;

#[test]
fn joins_answer_what_sqlite_answers() {
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    let (mut answered, mut refused, mut probed) = (0, 0, 0);
    for _ in 0..4 {
        let (db, lite) = load(&mut rng);
        for sql in statements(&mut rng) {
            let want = sorted(sqlite_rows(&lite, &sql));
            match fenec_rows(&db, &sql) {
                Ok(got) => {
                    assert_eq!(sorted(got), want, "{sql}");
                    answered += 1;
                    // A join read as a list may be read as the `lookup`'s
                    // probe, which the engine answers once the list would
                    // pass 100 000 values: answered here at once, held to
                    // SQLite's rows as well.
                    let Ok(Statement::Select(s)) = fenec_ql::parse_one(&sql) else {
                        continue;
                    };
                    if let Some(p) = s.probed() {
                        let got = statement_rows(&db, &Statement::Select(p))
                            .unwrap_or_else(|e| panic!("{sql}: its probe: {e}"));
                        assert_eq!(sorted(got), want, "{sql}: its probe");
                        probed += 1;
                    }
                }
                Err(e) => {
                    // A refusal names what to write; a missing field or a
                    // wrong row would be a bug.
                    assert!(
                        !e.contains("unknown") && !e.contains("not found"),
                        "{sql}: {e}"
                    );
                    refused += 1;
                }
            }
        }
    }
    eprintln!("{answered} answered as SQLite does, {refused} refused, {probed} probes");
    // Pinned: a reading that comes to refuse what it answered, or the
    // other way, says so here.
    assert_eq!(
        (answered, refused, probed),
        (ANSWERED, REFUSED, PROBED),
        "answered, refused, probed"
    );
}
