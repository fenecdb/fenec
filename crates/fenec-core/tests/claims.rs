//! A write that picks its rows and answers them: `set ... order ... limit n
//! returning *`, a job queue's claim. The rows are the ones a `get` with
//! the same `where`, `order` and `limit` answers, found and written under
//! the one writer's lock, so two workers never take the same job; a lease
//! is the job's ready time moved on, so one that lapses makes the job
//! claimable again; and `require 1` on the ack refuses a worker whose job
//! was claimed again in the meantime.

use fenec_core::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

/// 2026-05-03T09:20Z, the clock every test here starts at.
const T0: i64 = 1_777_800_000_000;

fn run(db: &mut Database, sql: &str) -> Result<Response> {
    db.execute(&fenec_ql::parse_one(sql).expect("parse"))
}

fn run_with(db: &mut Database, sql: &str, params: &[Value]) -> Result<Response> {
    db.execute_with(&fenec_ql::parse_one(sql).expect("parse"), params)
}

fn rows(r: Result<Response>) -> ResultSet {
    match r.unwrap() {
        Response::Rows(rs) => rs,
        r => panic!("{r:?}"),
    }
}

fn affected(r: Result<Response>) -> usize {
    match r.unwrap() {
        Response::Affected(n) => n,
        r => panic!("{r:?}"),
    }
}

fn ids(rs: &ResultSet) -> Vec<i64> {
    rs.rows.iter().map(|r| r.id as i64).collect()
}

fn text(s: &str) -> Value {
    Value::Text(s.into())
}

/// `n` jobs, the oldest first: job `i` ready a second after job `i - 1`,
/// the last at `T0`. `index` is the ready time's: `@sorted`, or none.
fn queue(n: i64, index: &str) -> Database {
    let mut db = Database::new();
    db.set_clock(Some(T0));
    run(
        &mut db,
        &format!(
            "create collection jobs (kind text, payload json, run_at timestamp {index}, \
             owner text, attempts int, error text, priority int)"
        ),
    )
    .unwrap();
    for i in 1..=n {
        let at = T0 - (n - i) * 1000;
        run(
            &mut db,
            &format!(
                "put jobs {{kind: \"mail\", payload: {{\"to\": \"u{i}\"}}, run_at: {at}, \
                 attempts: 0, priority: {}}}",
                i % 3
            ),
        )
        .unwrap();
    }
    db
}

/// The claim the docs show: ten of the oldest ready jobs, each the
/// worker's for 30 s.
const CLAIM: &str = "set jobs {owner: $1, run_at: now() + 30000, attempts: attempts + 1} \
                     where run_at <= now() order run_at limit $LIMIT returning *";

fn claim(db: &mut Database, owner: &str, limit: usize) -> ResultSet {
    let sql = CLAIM.replace("$LIMIT", &limit.to_string());
    rows(run_with(db, &sql, &[text(owner)]))
}

const ACK: &str = "del jobs where id = $1 and owner = $2 require 1";

#[test]
fn a_claim_takes_the_oldest_ready_jobs_and_answers_them_as_written() {
    let mut db = queue(20, "@sorted");
    let first = claim(&mut db, "w1", 3);
    assert_eq!(ids(&first), [1, 2, 3]);
    assert_eq!(
        first.columns,
        ["id", "kind", "payload", "run_at", "owner", "attempts", "error", "priority"]
    );
    for r in &first.rows {
        // As written: the owner, the lease's end, the attempt counted.
        assert_eq!(r.values[4], text("w1"));
        assert_eq!(r.values[3], Value::Timestamp(T0 + 30_000));
        assert_eq!(r.values[5], Value::Int(1));
    }
    // The next claim passes over them: their ready time is the lease's end.
    assert_eq!(ids(&claim(&mut db, "w2", 3)), [4, 5, 6]);
    // A list names the columns, a path among them.
    let r = rows(run(
        &mut db,
        "set jobs {owner: \"w3\"} where run_at <= now() order run_at limit 2 \
         returning id, payload.to, attempts",
    ));
    assert_eq!(r.columns, ["id", "payload.to", "attempts"]);
    assert_eq!(r.rows[0].values, [Value::Int(7), text("u7"), Value::Int(0)]);
    // Without `returning` a pick answers its count, as any `set` does.
    assert_eq!(
        affected(run(
            &mut db,
            "set jobs {owner: \"w4\"} where run_at <= now() order run_at limit 5"
        )),
        5
    );
}

#[test]
fn a_del_returning_pops_the_rows_as_they_were() {
    let mut db = queue(10, "@sorted");
    let popped = rows(run(
        &mut db,
        "del jobs order run_at desc limit 2 returning id, owner",
    ));
    assert_eq!(ids(&popped), [10, 9]);
    assert_eq!(popped.rows[0].values, [Value::Int(10), Value::Null]);
    assert_eq!(
        affected(run(
            &mut db,
            "del jobs where kind = \"mail\" order id limit 3"
        )),
        3
    );
    let left = rows(run(&mut db, "get jobs select id order id"));
    assert_eq!(ids(&left), [4, 5, 6, 7, 8]);
}

/// The rows a pick writes are the page a `get` with the same clauses
/// answers, through the ordered index or without one.
#[test]
fn a_pick_is_the_page_a_get_answers() {
    let picks = [
        "where run_at <= now() order run_at limit 7",
        "where run_at <= $1 order run_at desc limit 4",
        "where run_at <= now() - 30000 order run_at limit 100",
        "order priority desc, run_at limit 9",
        "order priority, id desc limit 5",
        "where priority = 1 order run_at limit 3",
        "where priority = 2 limit 6",
        "limit 4",
        "where run_at > now() order run_at limit 3",
        "where kind = \"mail\" and priority != 0 order run_at desc",
        "order run_at limit 0",
    ];
    for index in ["@sorted", ""] {
        for p in picks {
            let mut db = queue(60, index);
            let params = [Value::Timestamp(T0 - 20_000)];
            let page = rows(run_with(
                &mut db,
                &format!("get jobs select id {p}"),
                &params,
            ));
            let set = rows(run_with(
                &mut db,
                &format!("set jobs {{owner: \"x\"}} {p} returning id"),
                &params,
            ));
            assert_eq!(ids(&set), ids(&page), "{index} {p}");
            let owned = rows(run(
                &mut db,
                "get jobs select id where owner = \"x\" order id",
            ));
            let mut want = ids(&page);
            want.sort();
            assert_eq!(ids(&owned), want, "{index} {p}");
            let mut db = queue(60, index);
            let del = rows(run_with(
                &mut db,
                &format!("del jobs {p} returning id"),
                &params,
            ));
            assert_eq!(ids(&del), ids(&page), "{index} {p}");
        }
    }
}

/// `run_at <= now()` is a range of the ordered index, the time worked out
/// before the plan: the walk stops where the ready jobs end, rather than
/// testing every leased and delayed one after them and, past an eighth of
/// the collection, giving up for the scan.
#[test]
fn the_ready_jobs_are_a_range_of_the_index() {
    let mut db = queue(4_000, "@sorted");
    // All but three moved past now: leased or delayed.
    run(
        &mut db,
        "set jobs {run_at: now() + 60000} where run_at <= now() - 3000",
    )
    .unwrap();
    let plan = |db: &Database, sql: &str| -> String {
        let r = db
            .query(
                &fenec_ql::parse_one(&format!("explain {sql}")).unwrap(),
                &[],
            )
            .unwrap();
        let rs = r.rows().unwrap().clone();
        rs.rows
            .iter()
            .map(|r| format!("{:?}\n", r.values[0]))
            .collect()
    };
    let p = plan(&db, "get jobs where run_at <= now() order run_at limit 10");
    assert!(
        p.contains("walked the ordered index on run_at, 3 rows"),
        "{p}"
    );
    let p = plan(&db, "get jobs where run_at >= now() + 60000 - 1 count");
    assert!(p.contains("ordered index"), "{p}");
    assert_eq!(ids(&claim(&mut db, "w1", 10)), [3998, 3999, 4000]);
    // A time that does not work out stays as written: an error at the
    // first row, and none over no rows.
    let e = run(&mut db, "get jobs where run_at <= now() + \"x\" limit 1").unwrap_err();
    assert!(matches!(e, Error::Type(_)), "{e}");
    run(&mut db, "get jobs where id = 0 and run_at <= now() + \"x\"").unwrap();
}

#[test]
fn a_delayed_job_waits_for_its_time() {
    let mut db = queue(0, "@sorted");
    run(
        &mut db,
        &format!(
            "put jobs {{kind: \"later\", run_at: {}, attempts: 0}}",
            T0 + 60_000
        ),
    )
    .unwrap();
    run(
        &mut db,
        &format!("put jobs {{kind: \"now\", run_at: {T0}, attempts: 0}}"),
    )
    .unwrap();
    let got = claim(&mut db, "w1", 10);
    assert_eq!(got.rows.len(), 1);
    assert_eq!(got.rows[0].values[1], text("now"));
    run_with(
        &mut db,
        ACK,
        &[Value::Int(got.rows[0].id as i64), text("w1")],
    )
    .unwrap();
    db.set_clock(Some(T0 + 59_999));
    assert!(claim(&mut db, "w1", 10).rows.is_empty());
    db.set_clock(Some(T0 + 60_000));
    let got = claim(&mut db, "w1", 10);
    assert_eq!(got.rows.len(), 1);
    assert_eq!(got.rows[0].values[1], text("later"));
}

#[test]
fn a_lapsed_lease_is_claimed_again_and_the_first_owner_cannot_ack() {
    let mut db = queue(3, "@sorted");
    assert_eq!(ids(&claim(&mut db, "w1", 10)), [1, 2, 3]);
    // Extended, one lease outlives the others.
    let extend = "set jobs {run_at: now() + 30000} where id = $1 and owner = $2 require 1";
    db.set_clock(Some(T0 + 20_000));
    run_with(&mut db, extend, &[Value::Int(2), text("w1")]).unwrap();
    // w1 dies; the leases it did not extend lapse.
    db.set_clock(Some(T0 + 30_001));
    let again = claim(&mut db, "w2", 10);
    assert_eq!(ids(&again), [1, 3]);
    assert!(again.rows.iter().all(|r| r.values[5] == Value::Int(2)));
    // w1 back from the dead: its ack is refused, and nothing is deleted.
    let e = run_with(&mut db, ACK, &[Value::Int(1), text("w1")]).unwrap_err();
    assert!(matches!(e, Error::Unmet(_)), "{e}");
    // Its extended job is still its own.
    assert_eq!(
        affected(run_with(&mut db, ACK, &[Value::Int(2), text("w1")])),
        1
    );
    assert_eq!(
        affected(run_with(&mut db, ACK, &[Value::Int(1), text("w2")])),
        1
    );
    // Not an extension either, once the lease is gone.
    let e = run_with(&mut db, extend, &[Value::Int(3), text("w1")]).unwrap_err();
    assert!(matches!(e, Error::Unmet(_)), "{e}");
}

/// A failed job goes back with a backoff, and past its fifth attempt is a
/// dead letter: no ready time, so no claim finds it.
#[test]
fn a_failure_backs_off_and_the_fifth_is_a_dead_letter() {
    let mut db = queue(1, "@sorted");
    let fail = "set jobs {owner: null, error: $2, run_at: case when attempts >= 5 then null \
                else now() + 1000 * attempts * attempts end} where id = $1 and owner = $3 require 1";
    let mut now = T0;
    for attempt in 1..=5 {
        let got = claim(&mut db, "w1", 1);
        assert_eq!(ids(&got), [1], "attempt {attempt}");
        assert_eq!(got.rows[0].values[5], Value::Int(attempt));
        run_with(&mut db, fail, &[Value::Int(1), text("boom"), text("w1")]).unwrap();
        if attempt < 5 {
            // Not before its backoff.
            db.set_clock(Some(now + 1000 * attempt * attempt - 1));
            assert!(claim(&mut db, "w1", 1).rows.is_empty());
            now += 1000 * attempt * attempt;
            db.set_clock(Some(now));
        }
    }
    db.set_clock(Some(now + 86_400_000));
    assert!(claim(&mut db, "w1", 1).rows.is_empty());
    let dead = rows(run(
        &mut db,
        "get jobs select id, attempts, error where run_at is null",
    ));
    assert_eq!(dead.rows.len(), 1);
    assert_eq!(
        dead.rows[0].values,
        [Value::Int(1), Value::Int(5), text("boom")]
    );
    // Sent again by hand.
    run(
        &mut db,
        "set jobs {run_at: now(), attempts: 0, error: null} where id = 1 and run_at is null",
    )
    .unwrap();
    assert_eq!(ids(&claim(&mut db, "w2", 1)), [1]);
}

#[test]
fn require_counts_the_rows_a_pick_answers() {
    let mut db = queue(2, "@sorted");
    let one = "set jobs {owner: $1} where run_at <= now() and owner is null \
               order run_at limit 1 returning id require 1";
    assert_eq!(ids(&rows(run_with(&mut db, one, &[text("a")]))), [1]);
    assert_eq!(ids(&rows(run_with(&mut db, one, &[text("b")]))), [2]);
    let e = run_with(&mut db, one, &[text("c")]).unwrap_err();
    assert!(matches!(e, Error::Unmet(_)), "{e}");
    // `limit 2 require 2` refused over one row puts that row back.
    let mut db = queue(1, "@sorted");
    let e = run(
        &mut db,
        "set jobs {owner: \"x\"} order run_at limit 2 returning * require 2",
    )
    .unwrap_err();
    assert!(e.to_string().contains("wrote 1 row, and requires 2"), "{e}");
    let owner = rows(run(&mut db, "get jobs select owner"));
    assert_eq!(owner.rows[0].values, [Value::Null]);
}

#[test]
fn a_claim_in_a_block_put_back_is_claimable_again() {
    let mut db = queue(5, "@sorted");
    db.begin().unwrap();
    assert_eq!(ids(&claim(&mut db, "w1", 2)), [1, 2]);
    assert_eq!(ids(&claim(&mut db, "w1", 2)), [3, 4]);
    db.rollback();
    assert_eq!(ids(&claim(&mut db, "w2", 3)), [1, 2, 3]);
}

#[test]
fn returning_refuses_a_field_that_is_not_there_and_writes_nothing() {
    let mut db = queue(3, "@sorted");
    let e = run(
        &mut db,
        "set jobs {owner: \"x\"} where run_at <= now() order run_at limit 2 returning nope",
    )
    .unwrap_err();
    assert!(matches!(e, Error::NotFound(_)), "{e}");
    let e = run(&mut db, "del jobs order nope limit 1").unwrap_err();
    assert!(matches!(e, Error::NotFound(_)), "{e}");
    // Refused over no rows as well.
    let e = run(
        &mut db,
        "set jobs {owner: \"x\"} where id = 99 returning nope",
    )
    .unwrap_err();
    assert!(matches!(e, Error::NotFound(_)), "{e}");
    let left = rows(run(&mut db, "get jobs select id where owner is null"));
    assert_eq!(left.rows.len(), 3);
}

/// Sixteen threads claim batches of ten from one queue until it is empty,
/// acking nine in ten; the clock does not move meanwhile, so no lease
/// lapses, and any job handed out twice was claimed twice. Then the leases
/// lapse, and the jobs left are claimed again, each once.
#[test]
fn threads_claiming_from_one_queue_never_take_the_same_job() {
    const JOBS: i64 = 6_000;
    let db = Arc::new(Mutex::new(queue(JOBS, "@sorted")));
    let phase = |round: usize| -> Vec<i64> {
        let hs: Vec<_> = (0..16)
            .map(|t| {
                let db = db.clone();
                std::thread::spawn(move || {
                    let claim = fenec_ql::parse_one(&CLAIM.replace("$LIMIT", "10")).unwrap();
                    let ack = fenec_ql::parse_one(ACK).unwrap();
                    let mut got = Vec::new();
                    for n in 0.. {
                        let owner = text(&format!("{round}:{t}:{n}"));
                        let r = db
                            .lock()
                            .unwrap()
                            .execute_with(&claim, std::slice::from_ref(&owner))
                            .unwrap();
                        let Response::Rows(rs) = r else {
                            panic!("{r:?}")
                        };
                        if rs.rows.is_empty() {
                            break;
                        }
                        for row in &rs.rows {
                            let id = row.id as i64;
                            got.push(id);
                            // One in ten is dropped: its worker died.
                            if round == 0 && id % 10 == 0 {
                                continue;
                            }
                            let r = db
                                .lock()
                                .unwrap()
                                .execute_with(&ack, &[Value::Int(id), owner.clone()]);
                            assert_eq!(r.unwrap(), Response::Affected(1));
                        }
                    }
                    got
                })
            })
            .collect();
        hs.into_iter().flat_map(|h| h.join().unwrap()).collect()
    };
    let first = phase(0);
    let mut seen = HashSet::new();
    for id in &first {
        assert!(seen.insert(*id), "job {id} claimed twice under one lease");
    }
    assert_eq!(seen.len() as i64, JOBS, "every job claimed");
    db.lock().unwrap().set_clock(Some(T0 + 30_001));
    let second = phase(1);
    let mut again: Vec<i64> = second.clone();
    again.sort();
    let want: Vec<i64> = (1..=JOBS).filter(|i| i % 10 == 0).collect();
    assert_eq!(again, want, "the dropped jobs, each claimed once more");
    let left = rows(run(&mut db.lock().unwrap(), "get jobs count"));
    assert_eq!(left.rows[0].values[0], Value::Int(0));
    // At least once: every job was acked by some worker.
    let mut acked: HashMap<i64, usize> = HashMap::new();
    for id in first.iter().filter(|i| *i % 10 != 0).chain(&second) {
        *acked.entry(*id).or_default() += 1;
    }
    assert_eq!(acked.len() as i64, JOBS);
}
