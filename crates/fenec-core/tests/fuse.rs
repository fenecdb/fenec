//! `match ... near ... fuse`: the two rankings combined by reciprocal rank,
//! checked against the same sum worked out from each side run on its own.

use fenec_core::prelude::*;
use std::collections::HashMap;

fn exec(db: &mut Database, sql: &str, params: &[Value]) {
    db.execute_with(&fenec_ql::parse_one(sql).expect("parse"), params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn hits(db: &Database, sql: &str, params: &[Value]) -> Vec<(u64, f32)> {
    let r = db
        .query(&fenec_ql::parse_one(sql).expect("parse"), params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    r.rows()
        .unwrap()
        .rows
        .iter()
        .map(|r| (r.id, r.score.unwrap_or(0.0)))
        .collect()
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

const WORDS: [&str; 12] = [
    "vector", "search", "index", "graph", "text", "rank", "fusion", "query", "browser", "engine",
    "memory", "disk",
];

fn corpus() -> Database {
    let mut db = Database::new();
    exec(
        &mut db,
        "create collection d (body text @text, kind int, v vector<8> @hnsw(cosine, m=8))",
        &[],
    );
    let mut rng = Rng(0x51);
    for _ in 0..400 {
        let body: Vec<&str> = (0..6).map(|_| WORDS[rng.below(12) as usize]).collect();
        let v: Vec<f32> = (0..8)
            .map(|_| rng.below(1000) as f32 / 500.0 - 1.0)
            .collect();
        exec(
            &mut db,
            "put d {body: $1, kind: $2, v: $3}",
            &[
                Value::Text(body.join(" ")),
                Value::Int(rng.below(3) as i64),
                Value::Vector(v),
            ],
        );
    }
    db
}

/// Reciprocal rank fusion of two lists, by hand: ids by score, ties to the
/// lower id.
fn by_hand(text: &[(u64, f32)], vectors: &[(u64, f32)], k: f32) -> Vec<(u64, f32)> {
    let mut score: HashMap<u64, f32> = HashMap::new();
    for list in [text, vectors] {
        for (rank, (id, _)) in list.iter().enumerate() {
            *score.entry(*id).or_default() += 1.0 / (k + rank as f32 + 1.0);
        }
    }
    let mut out: Vec<(u64, f32)> = score.into_iter().collect();
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then(a.0.cmp(&b.0)));
    out
}

#[test]
fn fuse_is_the_reciprocal_rank_sum_of_both_sides() {
    let db = corpus();
    let q = [Value::Vector(vec![
        0.3, -0.2, 0.9, 0.1, -0.5, 0.4, 0.0, 0.2,
    ])];
    for (filter, depth, k) in [("", 50, 60.0), ("where kind = 1", 30, 60.0), ("", 40, 10.0)] {
        let text = hits(
            &db,
            &format!(r#"get d {filter} match body "fusion rank" limit {depth}"#),
            &q,
        );
        let vectors = hits(&db, &format!("get d {filter} near v $1 limit {depth}"), &q);
        let fused = hits(
            &db,
            &format!(
                r#"get d {filter} match body "fusion rank" near v $1 fuse k {k} candidates {depth} limit 10"#
            ),
            &q,
        );
        let want = by_hand(&text, &vectors, k);
        assert_eq!(fused.len(), 10);
        for ((id, s), (want_id, want_s)) in fused.iter().zip(&want) {
            assert_eq!(id, want_id, "{filter} k={k}: {fused:?} vs {want:?}");
            assert!((s - want_s).abs() < 1e-6);
        }
        // A page further down is the same ranking, cut later.
        let page = hits(
            &db,
            &format!(
                r#"get d {filter} match body "fusion rank" near v $1 fuse k {k} candidates {depth} limit 5 offset 5"#
            ),
            &q,
        );
        assert_eq!(page, fused[5..10]);
    }
    // The defaults are k = 60 and 20 candidates a side -- which is not the
    // answer 100 a side gives.
    let fused = hits(
        &db,
        r#"get d match body "graph" near v $1 fuse limit 10"#,
        &q,
    );
    let at = |depth: usize| {
        let text = hits(
            &db,
            &format!(r#"get d match body "graph" limit {depth}"#),
            &q,
        );
        let vectors = hits(&db, &format!("get d near v $1 limit {depth}"), &q);
        let mut all = by_hand(&text, &vectors, 60.0);
        all.truncate(10);
        all
    };
    let close = |a: &[(u64, f32)], b: &[(u64, f32)]| {
        a.len() == b.len()
            && a.iter()
                .zip(b)
                .all(|(x, y)| x.0 == y.0 && (x.1 - y.1).abs() < 1e-6)
    };
    assert!(close(&fused, &at(20)), "{fused:?} vs {:?}", at(20));
    assert!(!close(&fused, &at(100)));
}

#[test]
fn what_fuse_does_not_combine_with_is_refused() {
    let db = corpus();
    let q = [Value::Vector(vec![0.1; 8])];
    for (sql, says) in [
        (r#"get d match body "x" near v $1"#, "`fuse` ranks by both"),
        (r#"get d match body "x" fuse"#, "needs both"),
        (
            r#"get d match body "x" near v $1 fuse rerank v $1"#,
            "pick one",
        ),
        (
            r#"get d match body "x" near v $1 fuse order kind"#,
            "`fuse` cannot be combined with `order`",
        ),
        (
            r#"get d match body "x" near v $1 fuse candidates 0"#,
            "at least one",
        ),
        // Past the ceiling, a side or the page: refused rather than cut.
        (
            r#"get d match body "x" near v $1 fuse candidates 20000"#,
            "at most 10000 candidates a side",
        ),
        (
            r#"get d match body "x" near v $1 fuse limit 20000"#,
            "at most 10000 rows",
        ),
    ] {
        let err = match fenec_ql::parse_one(sql) {
            Err(e) => e.to_string(),
            Ok(stmt) => db.query(&stmt, &q).expect_err(sql).to_string(),
        };
        assert!(err.contains(says), "{sql}: {err}");
    }
    // `explain` names both sides and the fusion.
    let r = db
        .query(
            &fenec_ql::parse_one(r#"explain get d match body "graph" near v $1 fuse limit 3"#)
                .unwrap(),
            &q,
        )
        .unwrap();
    let plan: Vec<String> = r
        .rows()
        .unwrap()
        .rows
        .iter()
        .map(|r| match &r.values[0] {
            Value::Text(t) => t.clone(),
            v => panic!("{v:?}"),
        })
        .collect();
    for step in ["match:", "near:", "fuse: reciprocal rank, k = 60"] {
        assert!(plan.iter().any(|s| s.starts_with(step)), "{step}: {plan:?}");
    }
}
