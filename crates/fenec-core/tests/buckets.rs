//! A `@hash` bucket holds its ids ascending, whatever order they came in,
//! and gives the scan's answer row for row.
//!
//! A bucket was a list in the order its ids were added, and a row left it
//! by a walk of the whole list: a row written out of a bucket and back in
//! went last, so a page through the index was not the scan's page, and with
//! a field of few values every delete walked a fifth of the collection
//! (the `@ttl` sweep held the write lock 370 to 500 ms a thousand rows).

use fenec_core::prelude::*;

fn run(db: &mut Database, sql: &str) -> Response {
    db.execute(&fenec_ql::parse_one(sql).expect("parse"))
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn ids(db: &mut Database, sql: &str) -> Vec<u64> {
    run(db, sql)
        .rows()
        .expect("rows")
        .rows
        .iter()
        .map(|r| r.id)
        .collect()
}

#[test]
fn a_row_written_back_into_a_bucket_keeps_its_place_in_a_page() {
    let mut db = Database::new();
    run(&mut db, "create collection t (k text @hash, n int)");
    run(&mut db, "create collection plain (k text, n int)");
    for c in ["t", "plain"] {
        for i in 1..=4 {
            run(&mut db, &format!("put {c} {{id: {i}, k: \"a\", n: {i}}}"));
        }
        // Out of the bucket and back: it went to the end of the list.
        run(&mut db, &format!("set {c} {{k: \"b\"}} where id = 2"));
        run(&mut db, &format!("set {c} {{k: \"a\"}} where id = 2"));
    }
    let scan = ids(&mut db, "get plain where k = \"a\" limit 2");
    assert_eq!(scan, vec![1, 2]);
    assert_eq!(ids(&mut db, "get t where k = \"a\" limit 2"), scan);
    assert_eq!(
        ids(&mut db, "get t where k = \"a\""),
        ids(&mut db, "get plain where k = \"a\"")
    );
}

/// Rows of few values written, rewritten and deleted -- the oldest first, as
/// the sweep deletes, and here and there -- through buckets past a run's
/// size, each answer held to the same collection without the index.
#[test]
fn buckets_of_many_rows_answer_as_the_scan_through_deletes_and_rewrites() {
    let mut db = Database::new();
    run(&mut db, "create collection t (k text @hash, n int)");
    run(&mut db, "create collection plain (k text, n int)");
    let mut x: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let docs: Vec<String> = (0..6_000)
        .map(|i| format!("{{k: \"v{}\", n: {i}}}", next() % 5))
        .collect();
    for c in ["t", "plain"] {
        run(&mut db, &format!("put {c} [{}]", docs.join(", ")));
    }
    let check = |db: &mut Database| {
        for v in 0..5 {
            for tail in ["", " limit 7", " limit 3 offset 600"] {
                let a = ids(db, &format!("get t where k = \"v{v}\"{tail}"));
                let b = ids(db, &format!("get plain where k = \"v{v}\"{tail}"));
                assert_eq!(a, b, "k = v{v}{tail}");
            }
            let a = ids(
                db,
                &format!("get t where k in [\"v{v}\", \"v{}\"]", (v + 2) % 5),
            );
            let b = ids(
                db,
                &format!("get plain where k in [\"v{v}\", \"v{}\"]", (v + 2) % 5),
            );
            assert_eq!(a, b);
        }
    };
    check(&mut db);
    for round in 0..6 {
        let lo = round * 400;
        let r = next() % 5;
        let s = next() % 6_000;
        for c in ["t", "plain"] {
            run(
                &mut db,
                &format!("del {c} where n >= {lo} and n < {}", lo + 400),
            );
            run(
                &mut db,
                &format!("set {c} {{k: \"v{r}\"}} where n > {s} and n < {}", s + 300),
            );
            run(
                &mut db,
                &format!("del {c} where n > {} and n < {}", s / 2, s / 2 + 50),
            );
        }
        check(&mut db);
    }
}

/// Deleting the oldest rows of a collection whose `@hash` fields hold a
/// few values each costs about what it costs without the indexes: a row
/// leaves a bucket by a binary search and a run of at most 512 ids moved.
/// It walked the whole bucket -- 15 000 to 30 000 ids a field here -- and
/// 100 deletes took 91 ms against 0.61 in a debug build. Each side's best
/// of several rounds, the two taken in turns, so a busy machine slows both
/// rather than one.
#[test]
fn a_delete_out_of_a_large_bucket_costs_what_a_delete_without_one_does() {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection hashed (country text @hash, device text @hash, \
         browser text @hash, name text @hash, n int)",
    );
    run(
        &mut db,
        "create collection plain (country text, device text, browser text, name text, n int)",
    );
    let docs: Vec<String> = (0..60_000)
        .map(|i| {
            format!(
                "{{country: \"c{}\", device: \"d{}\", browser: \"b{}\", name: \"e{}\", n: {i}}}",
                i % 4,
                i % 3,
                i % 2,
                i % 4
            )
        })
        .collect();
    for c in ["hashed", "plain"] {
        for chunk in docs.chunks(5_000) {
            run(&mut db, &format!("put {c} [{}]", chunk.join(", ")));
        }
        // The indexes built, as the first read builds them.
        run(&mut db, &format!("get {c} where country = \"c0\" limit 1"));
    }
    let (mut hashed, mut plain) = (f64::MAX, f64::MAX);
    for round in 0..8 {
        for (c, best) in [("hashed", &mut hashed), ("plain", &mut plain)] {
            let t = std::time::Instant::now();
            for i in 0..100 {
                let n = round * 100 + i;
                run(&mut db, &format!("del {c} where id = {}", n + 1));
            }
            *best = best.min(t.elapsed().as_secs_f64());
        }
    }
    assert!(
        hashed < plain * 4.0,
        "100 deletes took {:.2} ms with four @hash fields, {:.2} ms without",
        hashed * 1e3,
        plain * 1e3
    );
}
