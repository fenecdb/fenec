//! `match` scored over the rows its filter selects (`Match::within`), as a
//! scoped token's is: every score is the one a collection holding those
//! rows alone gives, so nothing of the other rows reaches it. Scored over
//! the collection, a user's own memo went 9.87 -> 4.79 once another user
//! wrote 200 private ones holding the same word.

use fenec_core::prelude::*;

const WORDS: [&str; 12] = [
    "initech", "revenue", "memo", "budget", "sand", "dune", "oasis", "night", "report", "plan",
    "rust", "wasm",
];

struct Rng(u64);
impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

fn run(db: &mut Database, sql: &str) {
    for s in fenec_ql::parse(sql).unwrap_or_else(|e| panic!("{sql}: {e}")) {
        db.execute(&s).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

/// `(n, score)` of each row, in the order answered.
fn scores(db: &Database, sql: &str, within: bool) -> Vec<(i64, f32)> {
    let Statement::Select(mut sel) = fenec_ql::parse_one(sql).unwrap() else {
        panic!("{sql}");
    };
    sel.matcher.as_mut().unwrap().within = within;
    let Response::Rows(rs) = db.query(&Statement::Select(sel), &[]).unwrap() else {
        panic!("{sql}");
    };
    rs.rows
        .iter()
        .map(|r| match r.values[0] {
            Value::Int(n) => (n, r.score.unwrap()),
            ref v => panic!("{v:?}"),
        })
        .collect()
}

#[test]
fn a_filtered_match_within_scores_as_its_rows_alone_would() {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection memos (n int, owner text @hash, body text @text)",
    );
    for u in 0..4 {
        run(
            &mut db,
            &format!("create collection only{u} (n int, owner text @hash, body text @text)"),
        );
    }
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for n in 0..2_000 {
        // User 3 writes most, and most of it about initech.
        let u = if rng.below(4) == 0 { rng.below(3) } else { 3 };
        let len = 1 + rng.below(12);
        let body: Vec<&str> = (0..len)
            .map(|_| {
                if u == 3 && rng.below(2) == 0 {
                    "initech"
                } else {
                    WORDS[rng.below(WORDS.len() as u64) as usize]
                }
            })
            .collect();
        let doc = format!("{{n: {n}, owner: \"u{u}\", body: \"{}\"}}", body.join(" "));
        run(&mut db, &format!("put memos {doc}"));
        run(&mut db, &format!("put only{u} {doc}"));
    }
    for q in [
        "initech",
        "initech memo",
        "revenue budget plan",
        "sand dune oasis night",
        "rust",
        "nothing",
    ] {
        for u in 0..4 {
            let shared =
                format!("get memos select n where owner = \"u{u}\" match body \"{q}\" limit 50");
            let alone = format!("get only{u} select n match body \"{q}\" limit 50");
            let within = scores(&db, &shared, true);
            assert_eq!(within, scores(&db, &alone, false), "{q} as u{u}");
            if u < 3 && q.starts_with("initech") {
                // Over the collection, user 3's rows weigh on the others'.
                assert_ne!(within, scores(&db, &shared, false), "{q} as u{u}");
            }
        }
    }
    // With no filter the rows are the collection's, and so are the scores.
    let all = "get memos select n match body \"initech memo\" limit 20";
    assert_eq!(scores(&db, all, true), scores(&db, all, false));
}
