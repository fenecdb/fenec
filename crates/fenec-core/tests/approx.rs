//! `approx_count_distinct` counts in a sketch of fixed size where
//! `count(distinct ...)` keeps every value and stops at a million; a sketch
//! kept as bytes (`hll_accumulate`) merges with others (`hll_combine`), so
//! days counted apart count a month (`hll_estimate`).

use fenec_core::prelude::*;

fn run(db: &mut Database, sql: &str) {
    db.execute(&fenec_ql::parse_one(sql).expect("parse"))
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    let stmt = fenec_ql::parse_one(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    match db.query(&stmt, &[]) {
        Ok(Response::Rows(rs)) => rs.rows.into_iter().map(|r| r.values).collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int(n) => *n,
        other => panic!("{other:?}"),
    }
}

/// Visitors of 30 days, a few hundred to tens of thousands a day, each
/// day's estimate held to its exact count, and a month's -- from the rows
/// and from the days' sketches merged -- to the month's.
#[test]
fn an_estimate_is_within_its_error_by_day_and_merged_over_a_month() {
    let mut db = Database::new();
    run(&mut db, "create collection v (day int, user text)");
    run(
        &mut db,
        "create collection days (day int @hash, users bytes)",
    );
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    for day in 0..30u64 {
        let many = 200 + (day * day * 40) as usize;
        let docs: Vec<String> = (0..many)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                // Visitors come back across days: 60 000 of them in all.
                format!("{{day: {day}, user: \"u{}\"}}", x % 60_000)
            })
            .collect();
        for chunk in docs.chunks(5_000) {
            run(&mut db, &format!("put v [{}]", chunk.join(", ")));
        }
    }
    let exact = rows(
        &db,
        "get v select day, count(distinct user), approx_count_distinct(user) group day",
    );
    assert_eq!(exact.len(), 30);
    for r in &exact {
        let (n, e) = (int(&r[1]) as f64, int(&r[2]) as f64);
        assert!(
            (e - n).abs() / n < 0.03,
            "day {:?}: {n} visitors, {e} estimated",
            r[0]
        );
    }
    let month = int(&rows(&db, "get v select count(distinct user)")[0][0]) as f64;
    let approx = int(&rows(&db, "get v select approx_count_distinct(user)")[0][0]) as f64;
    assert!(
        (approx - month).abs() / month < 0.03,
        "{month} against {approx}"
    );

    // Each day's sketch kept as bytes, read out as JSON and put back as a
    // parameter's documents, the way a rollup would keep them.
    let sketches = rows(
        &db,
        "get v select day, hll_accumulate(user) as users group day",
    );
    let mut json = String::from("[[");
    for (i, r) in sketches.iter().enumerate() {
        if i > 0 {
            json.push(',');
        }
        json.push_str(&format!("{{\"day\": {}, \"users\": ", int(&r[0])));
        fenec_core::json::value_into(&mut json, &r[1]);
        json.push('}');
    }
    json.push_str("]]");
    let params = fenec_core::json::parse_params(&json).unwrap();
    db.execute_with(&fenec_ql::parse_one("put days $1").unwrap(), &params)
        .unwrap();
    // A month from the days: the merged sketch is the month's own, so the
    // estimate is the same number as from the rows.
    let merged = rows(&db, "get days select hll_estimate(hll_combine(users)) as n");
    assert_eq!(int(&merged[0][0]) as f64, approx);
    // A week of them, against the week's exact count.
    let week = int(&rows(&db, "get v select count(distinct user) where day >= 23")[0][0]) as f64;
    let from_days = int(&rows(
        &db,
        "get days select hll_estimate(hll_combine(users)) as n where day >= 23",
    )[0][0]) as f64;
    assert!(
        (from_days - week).abs() / week < 0.03,
        "{week} against {from_days}"
    );
    // One day's sketch alone, read as a row's value.
    let one = rows(
        &db,
        "get days select hll_estimate(users) as n where day = 29",
    );
    assert_eq!(one[0][0], exact[29][2]);
}

#[test]
fn small_and_empty_and_refused() {
    let mut db = Database::new();
    run(&mut db, "create collection t (g text, n int, b bytes)");
    // Nothing: 0, and no sketch.
    assert_eq!(
        rows(
            &db,
            "get t select approx_count_distinct(n), hll_accumulate(n) as s"
        ),
        [vec![Value::Int(0), Value::Null]]
    );
    run(
        &mut db,
        r#"put t [{g: "a", n: 1}, {g: "a", n: 1}, {g: "a", n: 2}, {g: "b", n: 7}, {g: "b"}]"#,
    );
    // Few values count exactly; nulls are not values.
    assert_eq!(
        rows(&db, "get t select g, approx_count_distinct(n) group g"),
        [
            vec![Value::Text("a".into()), Value::Int(2)],
            vec![Value::Text("b".into()), Value::Int(1)],
        ]
    );
    // What is not a sketch is refused, not counted.
    run(&mut db, "set t {b: \"nope\"} where g = \"a\"");
    let stmt = fenec_ql::parse_one("get t select hll_combine(b) as s").unwrap();
    let e = db.query(&stmt, &[]).unwrap_err().to_string();
    assert!(e.contains("not a sketch"), "{e}");
    let stmt = fenec_ql::parse_one("get t select hll_combine(n) as s").unwrap();
    let e = db.query(&stmt, &[]).unwrap_err().to_string();
    assert!(e.contains("takes sketches"), "{e}");
    let two = fenec_ql::parse_one("get t select approx_count_distinct(n, g) as x");
    assert!(two.map_or(true, |s| db.query(&s, &[]).is_err()));
}

/// A sketch a group, 16 KB once dense: past 64 MB of them the query is
/// refused, as `count(distinct ...)` is past a million values, never cut.
#[test]
fn too_many_groups_sketched_are_refused() {
    let mut db = Database::new();
    run(&mut db, "create collection t (g int, u int)");
    // 5 000 groups of 1 100 values: each sketch past its sparse quarter.
    for g in 0..5_000 {
        let docs: Vec<String> = (0..1_100).map(|u| format!("{{g: {g}, u: {u}}}")).collect();
        run(&mut db, &format!("put t [{}]", docs.join(", ")));
        if g == 50 {
            let r = rows(&db, "get t select g, approx_count_distinct(u) group g");
            assert_eq!(r.len(), 51);
        }
    }
    let stmt = fenec_ql::parse_one("get t select g, approx_count_distinct(u) group g").unwrap();
    let e = db.query(&stmt, &[]).unwrap_err().to_string();
    assert!(e.contains("64 MB"), "{e}");
}
