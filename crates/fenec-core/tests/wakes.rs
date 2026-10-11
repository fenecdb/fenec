//! `Database::wakes_at`: the next moment a filter's answer can change with
//! no write, which a server holding a claim (`Fenec-Wait`) sleeps until.
//! A delayed job's ready time, a lease's end, a row's `@ttl`, each turned
//! round or under `or`, `not` and an inner `get` -- and over generated
//! filters and rows, the matching set never changes before the moment
//! given, so a held claim is never woken late.

use fenec_core::prelude::*;

/// 2026-05-03T09:20Z.
const T0: i64 = 1_777_800_000_000;

fn run(db: &mut Database, sql: &str) {
    db.execute(&fenec_ql::parse_one(sql).expect("parse"))
        .unwrap_or_else(|e| panic!("`{sql}`: {e}"));
}

fn filter_of(sql: &str) -> (String, Option<Expr>) {
    match fenec_ql::parse_one(sql).expect("parse") {
        Statement::Update {
            collection, filter, ..
        }
        | Statement::Delete {
            collection, filter, ..
        } => (collection, filter),
        Statement::Select(sel) => (sel.collection, sel.filter),
        other => panic!("{other:?}"),
    }
}

fn wakes(db: &Database, sql: &str, params: &[Value], at: i64) -> Result<Option<i64>> {
    let (c, f) = filter_of(sql);
    db.wakes_at(&c, f.as_ref(), params, at)
}

/// Jobs ready at `T0 + each` milliseconds, kinds in turn `mail`, `sms`.
fn jobs(index: &str, at: &[i64]) -> Database {
    let mut db = Database::new();
    db.set_clock(Some(T0));
    run(
        &mut db,
        &format!("create collection jobs (kind text, run_at timestamp {index}, owner text)"),
    );
    for (i, ms) in at.iter().enumerate() {
        let kind = ["mail", "sms"][i % 2];
        run(
            &mut db,
            &format!("put jobs {{kind: \"{kind}\", run_at: {}}}", T0 + ms),
        );
    }
    db
}

const CLAIM: &str = "set jobs {owner: \"w\", run_at: now() + 30000} where run_at <= now() \
                     order run_at limit 1";

#[test]
fn a_delayed_job_wakes_a_claim_at_its_time_with_an_index_or_without() {
    for index in ["@sorted", ""] {
        let mut db = jobs(index, &[-1000, 5000, 9000]);
        // Ready now, so the claim takes it; its lease is the next moment
        // after the delayed jobs'.
        assert_eq!(wakes(&db, CLAIM, &[], T0).unwrap(), Some(T0 + 5000));
        run(&mut db, CLAIM);
        assert_eq!(wakes(&db, CLAIM, &[], T0).unwrap(), Some(T0 + 5000));
        db.set_clock(Some(T0 + 5000));
        run(&mut db, CLAIM);
        assert_eq!(wakes(&db, CLAIM, &[], T0 + 5000).unwrap(), Some(T0 + 9000));
        db.set_clock(Some(T0 + 9000));
        run(&mut db, CLAIM);
        // Each claimed job's lease ends 30 s on: the first claim's next.
        assert_eq!(wakes(&db, CLAIM, &[], T0 + 9000).unwrap(), Some(T0 + 30000));
        run(&mut db, "del jobs");
        assert_eq!(wakes(&db, CLAIM, &[], T0 + 9000).unwrap(), None, "{index}");
    }
}

/// A moment that came between the run (`at`) and the question is given as
/// it is, past: the server wakes at once rather than skip the row.
#[test]
fn a_moment_between_the_run_and_the_question_is_not_passed_over() {
    let mut db = jobs("@sorted", &[5000, 9000]);
    db.set_clock(Some(T0 + 7000));
    assert_eq!(wakes(&db, CLAIM, &[], T0).unwrap(), Some(T0 + 5000));
}

#[test]
fn each_way_of_writing_the_time_gives_its_own_moment() {
    let db = jobs("@sorted", &[1000]);
    let at = |sql: &str| wakes(&db, sql, &[], T0).unwrap();
    assert_eq!(at("del jobs where run_at <= now()"), Some(T0 + 1000));
    assert_eq!(at("del jobs where run_at < now()"), Some(T0 + 1001));
    assert_eq!(at("del jobs where now() >= run_at"), Some(T0 + 1000));
    assert_eq!(at("del jobs where now() > run_at"), Some(T0 + 1001));
    assert_eq!(at("del jobs where run_at <= now() - 5000"), Some(T0 + 6000));
    assert_eq!(at("del jobs where run_at <= now() + 500"), Some(T0 + 500));
    assert_eq!(at("del jobs where now() - 5000 >= run_at"), Some(T0 + 6000));
    assert_eq!(at("del jobs where 400 + now() >= run_at"), Some(T0 + 600));
    // Rows leaving count too: one ready until its time.
    assert_eq!(at("del jobs where run_at > now()"), Some(T0 + 1000));
    assert_eq!(at("del jobs where not (run_at > now())"), Some(T0 + 1000));
    assert_eq!(
        wakes(
            &db,
            "del jobs where run_at <= now() - $1",
            &[Value::Int(250)],
            T0
        )
        .unwrap(),
        Some(T0 + 1250)
    );
    // No time in it: nothing but a write changes it.
    assert_eq!(at("del jobs where kind = \"mail\""), None);
    assert_eq!(at("del jobs"), None);
}

/// A row whose value is the bound itself: `<` holds it a millisecond on,
/// `<=` already.
#[test]
fn a_row_on_the_bound_turns_a_millisecond_on_under_a_strict_comparison() {
    let db = jobs("@sorted", &[0, 1000]);
    let at = |sql: &str| wakes(&db, sql, &[], T0).unwrap();
    assert_eq!(at("del jobs where run_at < now()"), Some(T0 + 1));
    assert_eq!(at("del jobs where run_at >= now()"), Some(T0 + 1));
    assert_eq!(at("del jobs where run_at <= now()"), Some(T0 + 1000));
}

#[test]
fn a_time_read_another_way_is_refused() {
    let db = jobs("@sorted", &[1000]);
    for sql in [
        "del jobs where run_at = now()",
        "del jobs where run_at != now()",
        "del jobs where bucket(run_at, 60000) <= now()",
        "del jobs where run_at <= bucket(now(), 60000)",
        "del jobs where run_at <= now() * 2",
        "del jobs where run_at + 1000 <= now()",
        "del jobs where run_at in [now()]",
    ] {
        let e = wakes(&db, sql, &[], T0).expect_err(sql);
        assert!(matches!(e, Error::Query(_)), "{sql}: {e}");
    }
}

/// Every row's time counts, whatever the rest of the filter asks: a queue
/// beside another in one collection is woken by the other's jobs too,
/// early and never late.
#[test]
fn every_rows_time_counts_whatever_else_the_filter_asks() {
    let db = jobs("@sorted", &[1000, 3000]);
    let at = |sql: &str| wakes(&db, sql, &[], T0).unwrap();
    for sql in [
        "del jobs where run_at <= now() and kind = \"sms\"",
        "del jobs where kind = \"mail\" and run_at <= now()",
        "del jobs where run_at <= now() and kind = \"push\"",
        "del jobs where run_at <= now() or kind = \"sms\"",
    ] {
        assert_eq!(at(sql), Some(T0 + 1000), "{sql}");
    }
}

#[test]
fn rows_that_expire_and_inner_gets_have_their_moments() {
    let mut db = Database::new();
    db.set_clock(Some(T0));
    run(
        &mut db,
        "create collection holds (job int, at timestamp @ttl(10s))",
    );
    run(&mut db, "create collection jobs (run_at timestamp @sorted)");
    run(&mut db, &format!("put holds {{job: 1, at: {}}}", T0 - 4000));
    run(&mut db, &format!("put jobs {{run_at: {}}}", T0 + 20000));
    // A hold lapses 10 s after its time: a reaper of `expired()`, and any
    // write over the collection, wake then.
    assert_eq!(
        wakes(&db, "del holds where expired()", &[], T0).unwrap(),
        Some(T0 + 6000)
    );
    assert_eq!(wakes(&db, "del holds", &[], T0).unwrap(), Some(T0 + 6000));
    assert_eq!(
        wakes(
            &db,
            "del holds where job in (get jobs select id where run_at <= now())",
            &[],
            T0
        )
        .unwrap(),
        Some(T0 + 6000)
    );
    assert_eq!(
        wakes(
            &db,
            "set jobs {run_at: null} where id in (get holds select job where expired())",
            &[],
            T0
        )
        .unwrap(),
        Some(T0 + 6000)
    );
    assert_eq!(
        wakes(
            &db,
            "set holds {job: 2} where job in (get jobs select id where run_at <= now())",
            &[],
            T0 + 7000
        )
        .unwrap(),
        Some(T0 + 20000),
        "the hold is gone by then: the inner get's time is next"
    );
}

/// Generated rows and filters: walked from moment to moment, the rows a
/// filter holds at any time between two are the rows it held at the first,
/// and it is the moments alone where they change. A moment late would be a
/// claim held past a job it could take.
#[test]
fn the_answer_changes_at_no_time_before_the_moment_given() {
    let filters = [
        "run_at <= now()",
        "run_at < now() - 700",
        "now() + 1300 > run_at and kind = \"mail\"",
        "run_at <= now() or (kind = \"sms\" and run_at > now() + 2000)",
        "not (run_at >= now()) and owner is null",
        "(run_at <= now() and kind = \"sms\") or run_at < now() - 4000",
    ];
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut rand = move |n: i64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % n as u64) as i64
    };
    for index in ["@sorted", ""] {
        for round in 0..4 {
            // On a grid of 100 ms, as the offsets below are: rows share a
            // value, and a value lands on a bound, where `<` and `<=` part.
            let at: Vec<i64> = (0..40).map(|_| rand(120) * 100 - 2000).collect();
            let mut db = jobs(index, &at);
            for i in 0..10 {
                if rand(2) == 0 {
                    run(
                        &mut db,
                        &format!("set jobs {{owner: \"w\"}} where id = {}", i * 3 + 1),
                    );
                }
            }
            for f in filters {
                let get = fenec_ql::parse_one(&format!("get jobs where {f}")).unwrap();
                let held = |db: &mut Database, t: i64| -> Vec<u64> {
                    db.set_clock(Some(t));
                    match db.query(&get, &[]).unwrap() {
                        Response::Rows(rs) => rs.rows.iter().map(|r| r.id).collect(),
                        r => panic!("{r:?}"),
                    }
                };
                let del = format!("del jobs where {f}");
                let mut t = T0 - 3000;
                let mut moments = 0;
                loop {
                    let before = held(&mut db, t);
                    let next = wakes(&db, &del, &[], t).unwrap();
                    let end = next.unwrap_or(T0 + 20_000).min(T0 + 20_000);
                    assert!(end > t, "{f}: {end} after {t}");
                    // Every millisecond up to the moment, the same rows.
                    for s in [t + 1, (t + end) / 2, end - 1] {
                        if s > t && s < end {
                            assert_eq!(
                                held(&mut db, s),
                                before,
                                "{index} round {round} `{f}`: changed at {} before the moment {}",
                                s - T0,
                                end - T0
                            );
                        }
                    }
                    let Some(n) = next else { break };
                    if n >= T0 + 20_000 {
                        break;
                    }
                    t = n;
                    moments += 1;
                }
                assert!(moments > 0, "`{f}` met no moment");
            }
        }
    }
}
