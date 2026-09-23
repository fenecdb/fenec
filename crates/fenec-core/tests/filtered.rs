//! A filtered `near` finds its filter's rows only as far as its plan needs
//! them, and that must never change an answer.
//!
//! The reference is the plan as it ran before, spelled out through queries
//! whose paths the probe does not touch: find the whole set; search it
//! exactly when it is no larger than the ANN's budget (`ef * m0`); otherwise
//! take the unfiltered ANN's candidates in distance order and keep those in
//! the set; and search the set exactly when that comes up short.

use fenec_core::prelude::*;

/// `m = 8`, so `m0 = 16` and the budget is `ef * 16`.
const M0: usize = 16;
const EF_SEARCH: usize = 40;

fn exec(db: &mut Database, sql: &str) {
    db.execute(&fenec_ql::parse_one(sql).expect("parse"))
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn rows(db: &Database, sql: &str, params: &[Value]) -> Vec<(u64, Option<f32>)> {
    let r = db
        .query(&fenec_ql::parse_one(sql).expect("parse"), params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    r.rows()
        .expect("rows")
        .rows
        .iter()
        .map(|r| (r.id, r.score))
        .collect()
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn unit(&mut self) -> f32 {
        (self.next() % 20_000) as f32 / 10_000.0 - 1.0
    }
}

/// 3 000 rows in six clusters. `k` is spread evenly over the ids, `late`
/// grows with them (the newest rows hold the highest values), and `side`
/// marks one cluster, so it correlates with the vector.
fn collection() -> (Database, Vec<Value>) {
    let mut db = Database::new();
    exec(
        &mut db,
        &format!(
            "create collection c (k int, late int, side int, tag text @hash, \
             e vector<8> @hnsw(cosine, m=8, ef_construction=64, ef_search={EF_SEARCH}))"
        ),
    );
    let mut r = Rng(0x2545_F491_4F6C_DD1D);
    let centers: Vec<Vec<f32>> = (0..6).map(|_| (0..8).map(|_| r.unit()).collect()).collect();
    let n = 3000;
    let docs: Vec<Vec<(String, Expr)>> = (0..n)
        .map(|i| {
            let cluster = (r.next() % 6) as usize;
            let e: Vec<f32> = centers[cluster]
                .iter()
                .map(|c| c + r.unit() * 0.3)
                .collect();
            vec![
                ("k".into(), Expr::Lit(Value::Int((i % 100) as i64))),
                ("late".into(), Expr::Lit(Value::Int((i * 100 / n) as i64))),
                ("side".into(), Expr::Lit(Value::Int((cluster == 0) as i64))),
                (
                    "tag".into(),
                    Expr::Lit(Value::Text(["a", "b", "c"][i % 3].into())),
                ),
                ("e".into(), Expr::Lit(Value::Vector(e))),
            ]
        })
        .collect();
    db.execute(&Statement::Put {
        collection: "c".into(),
        docs,
    })
    .unwrap();
    // Queries near every cluster but the one `side` marks, so that filter
    // eliminates the ANN's candidates and forces the fallback.
    let queries = (1..6)
        .map(|c| Value::Vector(centers[c].iter().map(|x| x + r.unit() * 0.2).collect()))
        .collect();
    (db, queries)
}

/// What the whole set first gives, for `want` rows.
fn reference(
    db: &Database,
    filter: &str,
    q: &Value,
    ef: Option<usize>,
    want: usize,
) -> Vec<(u64, Option<f32>)> {
    let set: Vec<u64> = rows(db, &format!("get c select id {filter}"), &[])
        .into_iter()
        .map(|r| r.0)
        .collect();
    let exact = || {
        rows(
            db,
            &format!("get c select id {filter} near e $1 exact limit {want}"),
            std::slice::from_ref(q),
        )
    };
    let ef_used = ef.unwrap_or(EF_SEARCH);
    if set.len() <= ef_used * M0 {
        return exact();
    }
    let ef_clause = ef.map(|e| format!("ef {e}")).unwrap_or_default();
    let cands = rows(
        db,
        &format!(
            "get c select id near e $1 {ef_clause} limit {}",
            want.max(ef_used)
        ),
        std::slice::from_ref(q),
    );
    let hits: Vec<_> = cands
        .into_iter()
        .filter(|(id, _)| set.binary_search(id).is_ok())
        .take(want)
        .collect();
    if hits.len() < want.min(set.len()) {
        return exact();
    }
    hits
}

#[test]
fn a_probed_filter_gives_the_answer_the_whole_set_gave() {
    let (db, queries) = collection();
    let filters = [
        "where k < 1",      // 1%, found whole by the probe
        "where k < 30",     // 30%, spread over the ids
        "where k < 90",     // 90%
        "where late >= 95", // the newest 5%, at the end of the ids
        "where late < 3",   // the oldest 3%
        "where side = 1",   // one cluster: the ANN finds none of it
        "where side = 0 and k < 50",
        "where tag = \"b\"", // a whole hash bucket
        "where tag in [\"a\", \"c\"]",
        "where tag = \"a\" and k < 50", // a bucket, then tested
        "where k < 30 or side = 1",
        "where not (late < 50)",
        "where k < 0", // nothing
    ];
    let mut cases = 0;
    for filter in filters {
        for ef in [None, Some(1), Some(8), Some(200)] {
            for (limit, offset) in [(1, 0), (10, 0), (40, 0), (5, 3)] {
                for q in &queries {
                    let ef_clause = ef.map(|e| format!("ef {e}")).unwrap_or_default();
                    let sql = format!("get c select id {filter} near e $1 {ef_clause} limit {limit} offset {offset}");
                    let got = rows(&db, &sql, std::slice::from_ref(q));
                    let want: Vec<_> = reference(&db, filter, q, ef, limit + offset)
                        .into_iter()
                        .skip(offset)
                        .collect();
                    assert_eq!(got, want, "{sql}");
                    cases += 1;
                }
            }
        }
    }
    assert_eq!(cases, 13 * 4 * 4 * 5);
}
