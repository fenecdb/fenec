//! A file that compacts on its own: the dead bytes are counted as the
//! writes land, the policy says when, and the compact runs beside the
//! database while writes go on (`compact_when_due`, `Compactor`). Every
//! document has to come out as a twin that made the same writes and never
//! compacted holds it, and the file has to stay near what it holds.

#![cfg(all(feature = "std-fs", unix, target_pointer_width = "64"))]

use fenec_core::engine::{compact_when_due, CompactPolicy, Compactor, Garbage};
use fenec_core::prelude::*;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

fn exec(db: &mut Database, sql: &str) {
    db.execute(&fenec_ql::parse_one(sql).expect("parse"))
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn rows(db: &Database, sql: &str) -> Vec<(u64, Vec<Value>)> {
    let r = db
        .query(&fenec_ql::parse_one(sql).expect("parse"), &[])
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    r.rows()
        .unwrap()
        .rows
        .iter()
        .map(|r| (r.id, r.values.clone()))
        .collect()
}

fn tmp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fenecdb-autocompact-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{tag}.fenec"));
    let _ = std::fs::remove_file(&path);
    path
}

const SCHEMA: &str = "create collection c (tag text @hash, n int @sorted, \
                      body text @text, v vector<4> @hnsw(l2, m=8))";

const QUERIES: &[&str] = &[
    "get c",
    "get c where tag = \"t3\"",
    "get c select id, n where n >= 20 and n < 60 order n desc",
    "get c select id match body \"w7\"",
    "get c select id, n near v [0.5, 0.5, 0.5, 0.5] exact limit 10000",
    "get c count",
];

fn same(a: &Database, b: &Database) {
    for q in QUERIES {
        let (mut x, mut y) = (rows(a, q), rows(b, q));
        // The vectors repeat, and ties come out in the graph's order, which
        // a compact rebuilds: the same rows, in id order.
        if q.contains("near") {
            x.sort_by_key(|r| r.0);
            y.sort_by_key(|r| r.0);
        }
        assert_eq!(x, y, "{q}");
    }
}

/// xorshift64*: the writes are the same for the database and its twin.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// The next write: mostly a document written again, some new ones, some
/// deleted -- each body a few hundred bytes, so the dead pile up.
fn write(rng: &mut Rng, ids: u64) -> String {
    let id = 1 + rng.below(ids);
    let n = rng.below(100);
    let body = (0..30)
        .map(|k| format!("w{}", (n + k) % 13))
        .collect::<Vec<_>>()
        .join(" ");
    match rng.below(20) {
        0 => format!("del c where id = {id}"),
        1..=3 => format!(
            "put c {{id: {}, tag: \"t{}\", n: {n}, body: \"{body}\", v: [{}.0, 1.0, 0.5, 0.25]}}",
            id + ids,
            n % 7,
            n
        ),
        // A rewrite that leaves the vector, and one that changes it.
        4..=13 => format!("set c {{n: {n}, body: \"{body} again\"}} where id = {id}"),
        _ => format!(
            "put c {{id: {id}, tag: \"t{}\", n: {n}, body: \"{body}\", v: [{}.0, 0.5, 1.0, 0.25]}}",
            n % 7,
            rng.below(1000)
        ),
    }
}

fn seed(db: &mut Database, ids: u64) {
    exec(db, SCHEMA);
    for id in 1..=ids {
        exec(
            db,
            &format!(
                "put c {{id: {id}, tag: \"t{}\", n: {}, body: \"w{} seed text of a document\", v: [{}.0, 1.0, 0.0, 0.5]}}",
                id % 7,
                id % 100,
                id % 13,
                id
            ),
        );
    }
}

/// A policy that compacts a test's few megabytes.
fn small() -> CompactPolicy {
    CompactPolicy {
        ratio: 0.5,
        floor: 64 << 10,
    }
}

#[test]
fn the_dead_bytes_are_counted_as_writes_land_and_found_again_on_open() {
    for mapped in [true, false] {
        let path = tmp(&format!("counted-{mapped}"));
        let mut db = fenec_core::fs::open_with(&path, mapped, Box::new(Ok)).unwrap();
        seed(&mut db, 200);
        let fresh = db.garbage();
        let mut rng = Rng(7);
        for _ in 0..2_000 {
            let w = write(&mut rng, 200);
            exec(&mut db, &w);
        }
        db.sync().unwrap();
        let g = db.garbage();
        assert!(g.dead() > fresh.dead() + 100_000, "{fresh:?} -> {g:?}");
        assert_eq!(g.file, std::fs::metadata(&path).unwrap().len());
        drop(db);

        // The open counts the same from the records it walks.
        let mut db = fenec_core::fs::open_with(&path, mapped, Box::new(Ok)).unwrap();
        let again = db.garbage();
        assert_eq!(
            (again.file, again.live),
            (g.file, g.live),
            "mapped: {mapped}"
        );

        // A compact leaves nothing dead, and what it kept besides the
        // documents is not counted as dead after it.
        exec(&mut db, "compact");
        let after = db.garbage();
        assert_eq!(after.dead(), 0, "{after:?}");
        assert!(after.live <= g.live);
        assert_eq!(after.file, std::fs::metadata(&path).unwrap().len());
        assert!(after.file < g.file / 3, "{g:?} -> {after:?}");
    }
}

#[test]
fn the_policy_asks_for_both_a_share_and_a_floor() {
    let p = CompactPolicy::default();
    let g = |file: u64, live: u64| Garbage {
        file,
        live,
        kept: 0,
    };
    assert!(!p.due(g(100 << 20, 60 << 20)));
    assert!(p.due(g(200 << 20, 90 << 20)));
    // Half dead, but a few megabytes.
    assert!(!p.due(g(10 << 20, 4 << 20)));
    // What the last compact kept is not dead.
    assert!(!p.due(Garbage {
        file: 200 << 20,
        live: 90 << 20,
        kept: 30 << 20
    }));
    assert!(CompactPolicy::at(0.0).is_err());
    assert!(CompactPolicy::at(1.0).is_err());
    assert!(CompactPolicy::at(f64::NAN).is_err());
    assert_eq!(CompactPolicy::at(0.25).unwrap().ratio, 0.25);
}

/// Writes go on from one thread while a `Compactor` compacts the file
/// again and again beside them, mapped and read into memory: every
/// document comes out as the twin holds it, before and after the file is
/// opened again, and the file never grows far past what it holds.
#[test]
fn a_file_compacted_on_its_own_under_writes_holds_what_its_twin_does() {
    for mapped in [true, false] {
        let path = tmp(&format!("load-{mapped}"));
        let mut db = fenec_core::fs::open_with(&path, mapped, Box::new(Ok)).unwrap();
        let mut twin = Database::new();
        seed(&mut db, 300);
        seed(&mut twin, 300);
        let db = Arc::new(RwLock::new(db));
        let compactor = Compactor::start_every(&db, small(), Duration::from_millis(2)).unwrap();
        let mut rng = Rng(42);
        let mut most = 0f64;
        for i in 0..12_000 {
            let w = write(&mut rng, 300);
            exec(&mut db.write().unwrap(), &w);
            exec(&mut twin, &w);
            if i % 100 == 0 {
                let g = db.read().unwrap().garbage();
                most = most.max(g.file as f64 / (g.live + g.kept).max(1) as f64);
                // Now and then a pause, as a write load has: the compact
                // lands between writes, not only beside them.
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        drop(compactor);
        let compactions = db.read().unwrap().compactions();
        assert!(
            compactions >= 3,
            "mapped {mapped}: {compactions} compactions"
        );
        // Looked at every 2 ms, a compact due at half the file: the file is
        // never long past twice what it holds.
        assert!(most < 4.0, "mapped {mapped}: the file reached {most:.2}x");
        while let Some(r) = compact_when_due(&db, &small()) {
            r.unwrap();
        }
        let g = db.read().unwrap().garbage();
        assert!(!small().due(g), "{g:?}");

        same(&db.read().unwrap(), &twin);
        let db = Arc::try_unwrap(db).ok().unwrap().into_inner().unwrap();
        drop(db);
        let back = fenec_core::fs::open_with(&path, mapped, Box::new(Ok)).unwrap();
        same(&back, &twin);
    }
}

#[test]
fn nothing_is_due_while_a_maintenance_runs_or_after_a_storage_error() {
    let path = tmp("due");
    let mut db = fenec_core::fs::open(&path).unwrap();
    seed(&mut db, 100);
    let mut rng = Rng(3);
    for _ in 0..3_000 {
        let w = write(&mut rng, 100);
        exec(&mut db, &w);
    }
    assert!(db.compact_due(&small()));
    // A database with no file has nothing to compact beside.
    let mut plain = Database::new();
    seed(&mut plain, 100);
    for _ in 0..3_000 {
        let w = write(&mut rng, 100);
        exec(&mut plain, &w);
    }
    assert!(!plain.compact_due(&small()));

    exec(&mut db, "create collection d (x int)");
    let db = RwLock::new(db);
    let index = fenec_ql::parse_one("create index on d (x) @hash").unwrap();
    Database::maintain_with(&db, &index, &mut || {
        let g = db.read().unwrap();
        assert!(g.maintaining());
        assert!(!g.compact_due(&small()));
    })
    .unwrap()
    .unwrap();
    let mut g = db.write().unwrap();
    assert!(g.compact_due(&small()));
    g.fail(&Error::Io("the disk went away".into()));
    assert!(!g.compact_due(&small()));
}

/// A replica handed an image while it compacts -- it fell behind its
/// primary's buffer -- takes the image, and the compact, whose copies are
/// of the database before it, fails rather than put them back over it.
#[test]
fn an_image_adopted_during_a_compact_is_not_written_over() {
    let path = tmp("adopt");
    let mut db = fenec_core::fs::open(&path).unwrap();
    seed(&mut db, 100);
    let mut rng = Rng(11);
    for _ in 0..500 {
        let w = write(&mut rng, 100);
        exec(&mut db, &w);
    }
    let mut other = Database::new();
    seed(&mut other, 40);
    exec(&mut other, "set c {n: 99} where id < 10");
    let image = other.snapshot();
    let db = RwLock::new(db);
    let compact = fenec_ql::parse_one("compact").unwrap();
    for phase in 0..2 {
        let mut calls = 0;
        let r = Database::maintain_with(&db, &compact, &mut || {
            if calls == phase {
                let mut fresh = Database::new();
                fresh.load(&image).unwrap();
                db.write().unwrap().adopt(fresh, &image).unwrap();
            }
            calls += 1;
        });
        assert!(r.unwrap().is_err(), "adopted at the copy of phase {phase}");
        same(&db.read().unwrap(), &other);
    }
    let db = db.into_inner().unwrap();
    drop(db);
    same(&fenec_core::fs::open(&path).unwrap(), &other);
}

/// A compact beside the writes copies the versions written during it after
/// the image, which then holds the version each replaced, dead: counted as
/// dead, not as what the compact kept, or the next compact is due that much
/// later. Over the native library's thread, a compact during a burst of
/// updates of 100 rows left 101 KB of them counted as kept, and the file
/// stood at 307 KB, six times its 53 KB of rows, with nothing due.
#[test]
fn what_a_compact_beside_the_writes_leaves_dead_is_counted_dead() {
    for mapped in [true, false] {
        let path = tmp(&format!("beside-dead-{mapped}"));
        let mut db = fenec_core::fs::open_with(&path, mapped, Box::new(Ok)).unwrap();
        exec(&mut db, "create collection t (n int, body text)");
        let body = "x".repeat(500);
        for id in 1..=100 {
            exec(
                &mut db,
                &format!("put t {{id: {id}, n: 0, body: \"{body}\"}}"),
            );
        }
        for round in 1..=10 {
            exec(&mut db, &format!("set t {{n: {round}}} where id > 0"));
        }
        let db = RwLock::new(db);
        let compact = fenec_ql::parse_one("compact").unwrap();
        let mut round = 10;
        Database::maintain_with(&db, &compact, &mut || {
            round += 1;
            exec(
                &mut db.write().unwrap(),
                &format!("set t {{n: {round}}} where id > 0"),
            );
        })
        .unwrap()
        .unwrap();
        let last = round;
        assert!(last > 10, "no write landed during the compact");
        let mut db = db.into_inner().unwrap();
        db.sync().unwrap();
        let g = db.garbage();
        assert_eq!(db.compactions(), 1);
        assert_eq!(g.file, std::fs::metadata(&path).unwrap().len());
        // The schema and the counters: a few hundred bytes, not the 50 KB
        // of rows written over during the compact.
        assert!(g.kept < 4_096, "mapped {mapped}: {g:?}");
        assert!(g.dead() > 50_000, "mapped {mapped}: {g:?}");
        // The open counts them dead too, from the records it walks.
        drop(db);
        let again = fenec_core::fs::open_with(&path, mapped, Box::new(Ok)).unwrap();
        assert_eq!(
            (again.garbage().file, again.garbage().live),
            (g.file, g.live)
        );
        assert_eq!(rows(&again, &format!("get t where n = {last}")).len(), 100);
    }
}

/// A look at a compact that is due, made while a write holds the lock,
/// waits for it rather than pass the compact over until the next look:
/// under a steady writer the lock is taken at many a look, and a compact
/// passed over waited 5 s more while the file grew.
#[test]
fn a_look_made_while_a_write_holds_the_lock_waits_for_it() {
    let path = tmp("look-waits");
    let mut db = fenec_core::fs::open(&path).unwrap();
    seed(&mut db, 100);
    let db = Arc::new(RwLock::new(db));
    let g = {
        let mut g = db.write().unwrap();
        let mut rng = Rng(5);
        for _ in 0..3_000 {
            let w = write(&mut rng, 100);
            exec(&mut g, &w);
        }
        assert!(g.compact_due(&small()));
        g
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let looking = Arc::clone(&db);
    let look = std::thread::spawn(move || {
        let r = compact_when_due(&looking, &small()).map(|r| r.map(|_| ()));
        tx.send(()).unwrap();
        r
    });
    // The look cannot end while the lock is held: one that ended meanwhile
    // passed the compact over. Held a while, then let go.
    assert!(
        rx.recv_timeout(Duration::from_millis(200)).is_err(),
        "the look passed over a due compact while a write held the lock"
    );
    drop(g);
    assert!(matches!(look.join().unwrap(), Some(Ok(()))));
    assert_eq!(db.read().unwrap().compactions(), 1);
    assert!(!small().due(db.read().unwrap().garbage()));
}

/// A writer that never pauses: each compact still runs and ends -- its
/// catch-up copies the writes made meanwhile while the writer goes on, and
/// takes the write lock only for the last few -- and the file comes back to
/// what it holds each time rather than grow with every write.
#[test]
fn compacts_keep_up_with_a_writer_that_never_pauses() {
    for mapped in [true, false] {
        let path = tmp(&format!("steady-{mapped}"));
        let mut db = fenec_core::fs::open_with(&path, mapped, Box::new(Ok)).unwrap();
        let mut twin = Database::new();
        seed(&mut db, 300);
        seed(&mut twin, 300);
        let db = Arc::new(RwLock::new(db));
        let compactor = Compactor::start_every(&db, small(), Duration::from_millis(2)).unwrap();
        let mut rng = Rng(77);
        let (mut writes, mut most) = (0, 0f64);
        // Until five compacts have run, each statement parsed before the
        // lock is taken, as a server's are; bounded, should they never run.
        while db.read().unwrap().compactions() < 5 {
            assert!(writes < 500_000, "{writes} writes and no five compacts");
            let w = write(&mut rng, 300);
            let stmt = fenec_ql::parse_one(&w).unwrap();
            {
                let mut g = db.write().unwrap();
                g.execute(&stmt).unwrap();
                if writes % 100 == 0 {
                    let g = g.garbage();
                    most = most.max(g.file as f64 / (g.live + g.kept).max(1) as f64);
                }
            }
            twin.execute(&stmt).unwrap();
            writes += 1;
        }
        drop(compactor);
        // Never long past twice what it holds: 2.5 to 2.8x here, beside ten
        // busy loops on an 8-core machine as well.
        assert!(most < 4.0, "mapped {mapped}: the file reached {most:.2}x");
        while let Some(r) = compact_when_due(&db, &small()) {
            r.unwrap();
        }
        let g = db.read().unwrap().garbage();
        assert!(!small().due(g), "mapped {mapped}: {g:?}");
        same(&db.read().unwrap(), &twin);
    }
}
