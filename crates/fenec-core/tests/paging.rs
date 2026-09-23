//! A page is the front of the full answer, whatever the plan.
//!
//! Two shortcuts sit behind `limit`: a scan with no ordering stops at
//! `offset + limit` matches, and an ordering keeps only the top
//! `offset + limit` rows instead of sorting every match. Neither is allowed to
//! change a single row. The reference here is built outside the engine: every
//! matching row, sorted in the test with a stable sort, then sliced.

use fenec_core::prelude::*;

fn run(db: &mut Database, sql: &str) -> Response {
    db.execute(&fenec_ql::parse_one(sql).expect("parse"))
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// (id, a, b, tag) per row, in the order the engine returned them.
fn rows(db: &mut Database, sql: &str) -> Vec<(u64, i64, i64, String)> {
    run(db, sql)
        .rows()
        .expect("rows")
        .rows
        .iter()
        .map(|r| {
            let int = |v: &Value| match v {
                Value::Int(i) => *i,
                other => panic!("{other:?}"),
            };
            let text = match &r.values[2] {
                Value::Text(s) => s.clone(),
                other => panic!("{other:?}"),
            };
            (r.id, int(&r.values[0]), int(&r.values[1]), text)
        })
        .collect()
}

/// Few distinct values per field, so ties are everywhere and the tie order
/// is actually exercised.
fn fixture() -> Database {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection t (a int, b int, tag text @hash)",
    );
    let mut x: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let docs: Vec<String> = (0..3_000)
        .map(|_| {
            format!(
                "{{a: {}, b: {}, tag: \"t{}\"}}",
                next() % 7,
                next() % 50,
                next() % 5
            )
        })
        .collect();
    run(&mut db, &format!("put t [{}]", docs.join(", ")));
    // Deletions leave holes in the id sequence for the scan to walk over.
    run(&mut db, "del t where b = 13");
    db
}

#[test]
fn a_page_is_the_front_of_the_full_answer() {
    let mut db = fixture();
    let filters = [
        "",
        "where a >= 3",
        "where tag = \"t2\"",
        "where tag = \"t2\" and b < 20",
        "where tag in [\"t1\", \"t4\"]",
        "where a = 1 or b = 7",
        "where a = 99",
    ];
    // (clause, key extractor, ascending?) -- the test's own stable sort.
    type Key = fn(&(u64, i64, i64, String)) -> (i64, i64);
    let orders: [(&str, Option<(Key, bool)>); 6] = [
        ("", None),
        ("order a", Some((|r| (r.1, 0), true))),
        ("order a desc", Some((|r| (r.1, 0), false))),
        ("order b asc, a desc", Some((|r| (r.2, -r.1), true))),
        ("order a desc, b desc", Some((|r| (r.1, r.2), false))),
        ("order id desc", Some((|r| (r.0 as i64, 0), false))),
    ];
    let pages = [(0, 1), (0, 20), (5, 20), (100, 7), (2_990, 50), (0, 0)];

    for f in filters {
        // Every match, in id order: what the scan returns with no page.
        let all = rows(&mut db, &format!("get t select a, b, tag {f}"));
        assert!(
            all.windows(2).all(|w| w[0].0 < w[1].0),
            "{f}: not in id order"
        );
        for (o, key) in &orders {
            let mut reference = all.clone();
            if let Some((k, asc)) = key {
                // `sort_by` is stable: ties keep id order, as the engine's do.
                reference.sort_by(|x, y| {
                    let (kx, ky) = (k(x), k(y));
                    if *asc {
                        kx.cmp(&ky)
                    } else {
                        ky.cmp(&kx)
                    }
                });
            }
            let full = rows(&mut db, &format!("get t select a, b, tag {f} {o}"));
            assert_eq!(full, reference, "`{f} {o}` without a page");
            for (offset, limit) in pages {
                let sql = format!("get t select a, b, tag {f} {o} limit {limit} offset {offset}");
                let want: Vec<_> = reference.iter().skip(offset).take(limit).cloned().collect();
                assert_eq!(rows(&mut db, &sql), want, "{sql}");
            }
        }
    }
}

/// `count` and `required` are not pages of the scan: both still see every
/// match.
#[test]
fn count_and_required_still_see_every_match() {
    let mut db = fixture();
    let total = match &run(&mut db, "get t where a >= 3 count")
        .rows()
        .unwrap()
        .rows[0]
        .values[0]
    {
        Value::Int(n) => *n as usize,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        total,
        rows(&mut db, "get t select a, b, tag where a >= 3").len()
    );

    run(&mut db, "create collection c (parent int @hash, v int)");
    // Children only for the last parents, so a scan cut after the first few
    // candidates would find none of them.
    let ids: Vec<u64> = rows(&mut db, "get t select a, b, tag")
        .iter()
        .map(|r| r.0)
        .collect();
    let tail: Vec<String> = ids[ids.len() - 5..]
        .iter()
        .map(|id| format!("{{parent: {id}, v: 1}}"))
        .collect();
    run(&mut db, &format!("put c [{}]", tail.join(", ")));
    let page = run(&mut db, "get t limit 3 lookup c on parent required");
    let got: Vec<u64> = page.rows().unwrap().rows.iter().map(|r| r.id).collect();
    assert_eq!(got, ids[ids.len() - 5..ids.len() - 2].to_vec());
}
