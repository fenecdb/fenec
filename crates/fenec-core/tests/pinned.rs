//! A long read pinned under the read lock and run with none held
//! (`Database::pin`) answers what the read under the lock answered at the
//! pin: whatever lands after it -- deletes, updates of indexed fields, a
//! field added, renamed or dropped, a collection dropped, a compact's swap,
//! a handover -- and while writers write beside it. A read an index
//! answers is not pinned, and one that reached for an index all the same is
//! answered `None`, for the caller to run under the lock.

#![cfg(all(feature = "std-fs", unix, target_pointer_width = "64"))]

use fenec_core::prelude::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

fn exec(db: &mut Database, sql: &str) {
    db.execute(&fenec_ql::parse_one(sql).expect("parse"))
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn stmt(sql: &str) -> Statement {
    fenec_ql::parse_one(sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn tmp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fenecdb-pinned-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{tag}.fenec"));
    let _ = std::fs::remove_file(&path);
    path
}

/// Events with a hash, an ordered and a text index beside the fields the
/// long reads read, and accounts an inner `get` reads.
fn seeded(db: &mut Database) {
    exec(
        db,
        "create collection ev (user int @hash, name text, at int @sorted, \
         country text, n int, body text @text)",
    );
    exec(db, "create collection acct (owner int, kind text)");
    let names = ["view", "click", "buy", "signup"];
    let countries = ["TR", "US", "DE"];
    let put = stmt("put ev {user: $1, name: $2, at: $3, country: $4, n: $5, body: $6}");
    db.begin().unwrap();
    for i in 0..3000i64 {
        let args = [
            Value::Int(i % 97),
            Value::Text(names[(i % 4) as usize].into()),
            Value::Int(1_000 + i),
            Value::Text(countries[(i % 3) as usize].into()),
            Value::Int(i % 11),
            Value::Text(format!("word{} common", i % 13)),
        ];
        db.execute_with(&put, &args).unwrap();
    }
    for i in 0..97 {
        exec(
            db,
            &format!("put acct {{owner: {i}, kind: \"{}\"}}", ["a", "b"][i % 2]),
        );
    }
    db.commit().unwrap();
}

/// The long reads a pin takes: aggregates, a count, a scan, an order no
/// index walks, a facet that reads the rows, an inner `get`, a read-only
/// batch's three.
const LONG: &[&[&str]] = &[
    &["get ev select name, count(*), sum(n), min(at), max(at) group name"],
    &["get ev select user, count(distinct country) as c group user having c > 1 count"],
    &["get ev where country ~ \"R\" and n > 3 count"],
    &["get ev select id, n, country where n > 7 order country desc, n limit 50 offset 3"],
    &["get ev where n = 2 count facet country"],
    &["get ev select name, count(*) where n in (get acct select owner where kind = \"a\") group name"],
    &[
        "get ev select country, sum(n) group country",
        "get ev count",
        "get acct select kind, count(*) group kind",
    ],
    &["get ev"],
];

fn pairs(stmts: &[Statement]) -> Vec<(&Statement, &[Value])> {
    stmts.iter().map(|s| (s, &[][..])).collect()
}

/// What `reads` answer under the lock now, and the pin taken of them at the
/// same moment.
fn pin_beside(db: &Database, reads: &[Statement]) -> (Vec<Result<Response>>, Pinned) {
    let expected = reads.iter().map(|s| db.query(s, &[])).collect();
    let p = (db.pin(&pairs(reads))).unwrap_or_else(|| panic!("{reads:?} is not pinned"));
    (expected, p)
}

fn answered(p: &Pinned, reads: &[Statement]) -> Vec<Result<Response>> {
    reads
        .iter()
        .map(|s| p.query(s, &[]).expect("a read the pin holds"))
        .collect()
}

#[test]
fn a_pinned_read_answers_as_at_the_pin_whatever_lands_after() {
    let mut db = fenec_core::fs::open(tmp("after")).unwrap();
    db.set_pin_at(0);
    seeded(&mut db);
    let reads: Vec<Vec<Statement>> = LONG
        .iter()
        .map(|r| r.iter().map(|s| stmt(s)).collect())
        .collect();
    let pins: Vec<_> = reads.iter().map(|r| pin_beside(&db, r)).collect();
    let at = db.change_seq();
    // Deletes, updates of indexed fields and of the fields read, new rows,
    // a compact under the lock, then the schema: a field added, renamed,
    // dropped, an index built, the inner `get`'s collection dropped.
    exec(&mut db, "del ev where n = 4");
    exec(
        &mut db,
        "set ev {user: 1000, at: 5, name: \"gone\"} where n = 9",
    );
    exec(
        &mut db,
        "set ev {country: \"FR\"} where country = \"TR\" and n < 3",
    );
    exec(
        &mut db,
        "put ev {user: 1, name: \"view\", at: 99, country: \"JP\", n: 1}",
    );
    exec(&mut db, "compact");
    exec(&mut db, "alter collection ev add field extra int");
    exec(&mut db, "alter collection ev rename field country to land");
    exec(&mut db, "alter collection ev drop field n");
    exec(&mut db, "create index on ev (name) @hash");
    exec(&mut db, "drop collection acct");
    for ((expected, p), r) in pins.iter().zip(&reads) {
        assert_eq!(&answered(p, r), expected, "{r:?}");
    }
    // A checkpoint rewrites the file and points the stores at it; the
    // pins keep reading the one they took.
    db.checkpoint().unwrap();
    for ((expected, p), r) in pins.iter().zip(&reads) {
        assert_eq!(&answered(p, r), expected, "{r:?}");
    }
    // Each answers at the change it was pinned at.
    assert!(db.change_seq() > at);
    assert!(pins.iter().all(|(_, p)| p.change_seq() == at));
}

#[test]
fn a_pinned_read_outlives_a_handover_and_a_compact_beside() {
    let path = tmp("handover");
    let mut db = fenec_core::fs::open(&path).unwrap();
    db.set_pin_at(0);
    // Every write handed over to the file, the segments let go of.
    db.set_handover(1);
    seeded(&mut db);
    let lock = RwLock::new(db);
    let reads: Vec<Statement> = LONG[0..4].iter().map(|r| stmt(r[0])).collect();
    let (expected, p) = pin_beside(&lock.read().unwrap(), &reads);
    {
        let mut db = lock.write().unwrap();
        for i in 0..200 {
            exec(
                &mut db,
                &format!("set ev {{n: {}}} where id = {}", i % 5, i + 1),
            );
        }
        exec(&mut db, "del ev where n = 3");
    }
    assert_eq!(answered(&p, &reads), expected);
    let compact = stmt("compact");
    Database::maintain(&lock, &compact)
        .expect("a compact runs beside")
        .unwrap();
    assert_eq!(answered(&p, &reads), expected);
}

#[test]
fn an_index_read_is_not_pinned_and_short_reads_are_not_either() {
    let mut db = Database::new();
    seeded(&mut db);
    db.set_pin_at(100);
    let declined = [
        // An index answers each: a hash bucket, an ordered range, an
        // ordered walk, a facet the buckets count, `match`, an inner `get`
        // a bucket answers, `expired()`'s range.
        "get ev where user = 3 count",
        "get ev where user in [1, 2] select name, count(*) group name",
        "get ev where at >= 2000 count",
        "get ev order at desc limit 5",
        "get ev where n = 1 count facet user",
        "get ev match body \"common\" limit 5",
        "get ev where user in (get acct select owner) count",
        // Short: by id, a page with no order, a count of everything, a
        // statement that is not a `get`.
        "get ev where id = 7",
        "get ev where n = 3 limit 10",
        "get ev limit 50",
        "get ev count",
        "collections",
    ];
    for sql in declined {
        let s = stmt(sql);
        assert!(db.pin(&[(&s, &[][..])]).is_none(), "{sql}");
    }
    // One short read beside a long one in a batch is pinned with it.
    let batch = [
        stmt("get ev where id = 7"),
        stmt("get ev count where n > 1"),
    ];
    let p = db.pin(&pairs(&batch)).expect("the batch is long");
    assert_eq!(
        answered(&p, &batch),
        batch.iter().map(|s| db.query(s, &[])).collect::<Vec<_>>()
    );
    // Fewer rows than the bound: under the lock.
    db.set_pin_at(10_000);
    assert!(db.pin(&pairs(&batch)).is_none());
}

/// A range written with the time -- `at >= now() - 500` -- is a range of
/// the ordered index once the time is worked out, as the read works it
/// out before its plan: declined as `at >= 2000` is, where judged as
/// written it was pinned and, meeting the index, ran again under the lock.
#[test]
fn a_range_of_the_time_is_not_pinned() {
    let mut db = Database::new();
    exec(
        &mut db,
        "create collection tick (at timestamp @sorted, n int)",
    );
    db.begin().unwrap();
    for i in 0..3000 {
        exec(
            &mut db,
            &format!("put tick {{at: {}, n: {}}}", 1_000 + i, i % 7),
        );
    }
    db.commit().unwrap();
    db.set_pin_at(100);
    db.set_clock(Some(2_500));
    for sql in [
        "get tick where at >= now() - 500 count",
        "get tick where at >= 2000 count",
    ] {
        let s = stmt(sql);
        assert!(db.pin(&[(&s, &[][..])]).is_none(), "{sql}");
    }
    // A long read beside it is still pinned, and answers as the lock does.
    let s = stmt("get tick select n, count(*) where n >= 3 group n");
    let p = db.pin(&[(&s, &[][..])]).expect("a long read");
    assert_eq!(
        answered(&p, std::slice::from_ref(&s)),
        vec![db.query(&s, &[])]
    );
}

/// A read `pin` would have declined, run on a pin all the same -- every
/// collection taken, the shape not asked -- meets an index that refuses
/// and is answered `None`, never planned without the index; one that reads
/// the documents alone is answered as the lock answers it.
#[test]
fn a_read_that_reaches_for_an_index_is_answered_none() {
    let mut db = Database::new();
    exec(
        &mut db,
        "create collection t (k int @hash, n int @sorted, body text @text, \
         s sparse<8> @inverted, at timestamp @ttl(1d))",
    );
    exec(&mut db, "create collection u (k int @hash)");
    for i in 0..50 {
        let put = format!(
            "put t {{k: {}, n: {i}, body: \"w{} common\", s: \"{{1:0.5}}/8\"}}",
            i % 5,
            i % 3
        );
        exec(&mut db, &put);
        exec(&mut db, &format!("put u {{k: {i}}}"));
    }
    db.set_pin_at(0);
    let p = db.pin_every_collection();
    for sql in [
        "get t where k = 1",
        "get t where k in [1, 2] count",
        "get t where n > 3 count",
        "get t order n limit 3",
        "get t where n < 9 count facet k",
        "get t count facet n ranges [0, 10, 20]",
        "get t match body \"common\" limit 3",
        "get t near s \"{1:1}/8\" limit 3",
        "get t where k in (get u select k where k < 3) count",
        "get t where expired() count",
        "get t limit 3 lookup u on k = k",
    ] {
        let s = stmt(sql);
        assert!(db.pin(&[(&s, &[][..])]).is_none(), "{sql}");
        assert_eq!(p.query(&s, &[]), None, "{sql}");
    }
    for sql in [
        "get t where body ~ \"w1\" count",
        "get t select k, count(*), sum(n) group k",
        "get t where n in (get u select k where id < 5) count",
    ] {
        let s = stmt(sql);
        assert_eq!(p.query(&s, &[]), Some(db.query(&s, &[])), "{sql}");
    }
}

#[test]
fn the_pins_are_held_to_their_budget() {
    let mut db = Database::new();
    seeded(&mut db);
    db.set_pin_at(0);
    let read = [stmt(LONG[0][0])];
    let one = db.pin(&pairs(&read)).expect("pinned");
    let held = one.held_bytes();
    assert!(held >= 3000 * 12, "{held}");
    db.set_pin_budget(held * 2);
    let two = db.pin(&pairs(&read)).expect("within the budget");
    assert!(db.pin(&pairs(&read)).is_none(), "past the budget");
    drop(two);
    let three = db.pin(&pairs(&read)).expect("its bytes given back");
    drop((one, three));
}

/// Writers changing every kind of thing a read reads, compacts beside them
/// and handovers, while readers pin and read: each pinned answer is the one
/// the lock gave at the pin.
#[test]
fn pinned_reads_beside_writers_answer_as_at_the_pin() {
    let mut db = fenec_core::fs::open(tmp("beside")).unwrap();
    db.set_pin_at(0);
    db.set_handover(64 << 10);
    seeded(&mut db);
    let db = Arc::new(RwLock::new(db));
    let stop = AtomicBool::new(false);
    let compared = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for w in 0..3u64 {
            let (db, stop) = (Arc::clone(&db), &stop);
            s.spawn(move || {
                let mut x = w + 1;
                let mut step = || {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    x
                };
                while !stop.load(Ordering::Relaxed) {
                    let r = step();
                    let sql = match r % 6 {
                        0 => format!("del ev where id = {}", r % 3000 + 1),
                        1 => format!("set ev {{user: {}, at: {}}} where n = {}", r % 50, r % 9000, r % 11),
                        2 => format!(
                            "put ev {{user: {}, name: \"buy\", at: {}, country: \"US\", n: {}}}",
                            r % 97,
                            r % 5000,
                            r % 11
                        ),
                        3 => format!("set ev {{name: \"view\", n: n + 1}} where id = {}", r % 3000 + 1),
                        4 => format!("set acct {{kind: \"{}\"}} where owner = {}", ["a", "b"][(r % 2) as usize], r % 97),
                        _ => format!("put ev {{id: {}, user: 5, name: \"click\", at: 7, country: \"DE\", n: 2}}", r % 3000 + 1),
                    };
                    let mut g = db.write().unwrap();
                    g.execute(&stmt(&sql)).unwrap();
                }
            });
        }
        {
            let (db, stop) = (Arc::clone(&db), &stop);
            s.spawn(move || {
                let compact = stmt("compact");
                while !stop.load(Ordering::Relaxed) {
                    Database::maintain(&db, &compact).unwrap().unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            });
        }
        for r in 0..3usize {
            let (db, stop, compared) = (Arc::clone(&db), &stop, &compared);
            s.spawn(move || {
                let mut i = r;
                while !stop.load(Ordering::Relaxed) {
                    let reads: Vec<Statement> =
                        LONG[i % LONG.len()].iter().map(|s| stmt(s)).collect();
                    i += 1;
                    let (expected, p) = pin_beside(&db.read().unwrap(), &reads);
                    // Let writes land between the pin and the read.
                    std::thread::sleep(std::time::Duration::from_millis(2));
                    assert_eq!(answered(&p, &reads), expected, "{reads:?}");
                    compared.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
        stop.store(true, Ordering::Relaxed);
    });
    assert!(compared.load(Ordering::Relaxed) > 20);
}
