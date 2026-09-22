//! `--follow` against a live server: every kind of change arrives, a broken
//! stream loses nothing, a stopped follower resumes without a copy, and a
//! copy cut short is made again.
//!
//! Marked `#[ignore]`: it needs PostgreSQL with `wal_level=logical`, which
//! the Makefile's server has --
//!
//! ```text
//! make pgvector-up
//! cargo test -p fenec-import --test follow -- --ignored
//! make pgvector-down
//! ```
//!
//! Each test has its own table, slot and publication, so they run in
//! parallel; each drops its slot at the end, or the server would keep its
//! log for a follower that never comes back.

use fenec_core::prelude::*;
use fenec_import::follow::{self, Event, Follow};
use fenec_import::pg::Url;
use fenec_import::Options;
use fenec_pg::client::Client;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const DEFAULT_URL: &str = "postgres://postgres:fenec@127.0.0.1:55432/fenecbench";

fn url() -> Url {
    let s = std::env::var("FENEC_TEST_PG_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    Url::parse(&s).expect("invalid FENEC_TEST_PG_URL")
}

fn sql(q: &str) {
    let mut c = Client::connect(&url()).expect("could not connect (make pgvector-up?)");
    c.query(q).unwrap_or_else(|e| panic!("{q}: {e}"));
}

/// A table of `rows` rows with a key, a title, a number, tags and a vector
/// of 768 -- 3 KB, so PostgreSQL TOASTs it and an update of the title
/// arrives without it.
fn table(name: &str, rows: usize) {
    let _ = Client::connect(&url())
        .unwrap()
        .query("create extension if not exists vector");
    for q in [
        format!("select pg_drop_replication_slot('{name}') from pg_replication_slots where slot_name = '{name}'"),
        format!("drop publication if exists {name}"),
        format!("drop table if exists {name}"),
        format!(
            "create table {name} (id bigint primary key, title text, n int, tags text[], embed vector(768))"
        ),
        format!(
            "insert into {name} select g, 'row ' || g, g, array['a', 'b'], \
             (select array_agg(sin(g * 1000 + i))::vector(768) from generate_series(1, 768) i) \
             from generate_series(1, {rows}) g"
        ),
    ] {
        sql(&q);
    }
}

struct Follower {
    db: Arc<RwLock<Database>>,
    stop: Arc<AtomicBool>,
    events: Arc<Mutex<Vec<String>>>,
    thread: Option<JoinHandle<fenec_core::error::Result<()>>>,
}

impl Follower {
    fn start(name: &str, db: Arc<RwLock<Database>>) -> Follower {
        let stop = Arc::new(AtomicBool::new(false));
        let events = Arc::new(Mutex::new(Vec::new()));
        let (s, e, d, name) = (
            Arc::clone(&stop),
            Arc::clone(&events),
            Arc::clone(&db),
            name.to_string(),
        );
        let thread = std::thread::spawn(move || {
            let follow = Follow {
                slot: name.clone(),
                publication: name.clone(),
            };
            let opts = Options::new(name.clone());
            let mut report = |ev: Event| e.lock().unwrap().push(format!("{ev:?}"));
            follow::run(&url(), &name, &d, &opts, &follow, &s, &mut report)
        });
        Follower {
            db,
            stop,
            events,
            thread: Some(thread),
        }
    }

    fn saw(&self, what: &str) -> bool {
        self.events.lock().unwrap().iter().any(|e| e.contains(what))
    }

    fn stop(mut self) -> Vec<String> {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap().unwrap();
        let events = self.events.lock().unwrap().clone();
        events
    }
}

/// The table as the server has it, by id: title, n, tags, and the vector.
type Rows = Vec<(
    i64,
    Option<String>,
    Option<i64>,
    Option<String>,
    Option<Vec<f32>>,
)>;

fn source_rows(name: &str) -> Rows {
    let mut c = Client::connect(&url()).unwrap();
    let r = c
        .query(&format!(
            "select id, title, n, array_to_string(tags, ','), embed::text from {name} order by id"
        ))
        .unwrap();
    r.rows
        .iter()
        .map(|row| {
            let embed = row[4].as_ref().map(|s| {
                s.trim_matches(|c| c == '[' || c == ']')
                    .split(',')
                    .map(|x| x.parse::<f32>().unwrap())
                    .collect()
            });
            (
                row[0].as_ref().unwrap().parse().unwrap(),
                row[1].clone(),
                row[2].as_ref().map(|s| s.parse().unwrap()),
                row[3].clone(),
                embed,
            )
        })
        .collect()
}

fn mirror_rows(db: &RwLock<Database>, name: &str) -> Rows {
    let g = db.read().unwrap();
    let Ok(c) = g.collection(name) else {
        return Vec::new();
    };
    let mut ids = c.store.ids();
    ids.sort_unstable();
    ids.iter()
        .map(|&id| {
            let d = c.store.read(&c.schema, id).unwrap().unwrap();
            let text = |f: &str| match d.get(f) {
                Some(Value::Text(s)) => Some(s.clone()),
                _ => None,
            };
            let tags = match d.get("tags") {
                Some(Value::List(items)) => Some(
                    items
                        .iter()
                        .map(|v| match v {
                            Value::Text(s) => s.clone(),
                            other => format!("{other:?}"),
                        })
                        .collect::<Vec<_>>()
                        .join(","),
                ),
                _ => None,
            };
            let n = match d.get("n") {
                Some(Value::Int(n)) => Some(*n),
                _ => None,
            };
            let embed = match d.get("embed") {
                Some(Value::Vector(v)) => Some(v.clone()),
                _ => None,
            };
            (id as i64, text("title"), n, tags, embed)
        })
        .collect()
}

/// Waits until the mirror holds exactly the server's rows.
fn converge(db: &RwLock<Database>, name: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (want, got) = (source_rows(name), mirror_rows(db, name));
        if want == got {
            return;
        }
        if Instant::now() > deadline {
            let first = want
                .iter()
                .zip(&got)
                .find(|(a, b)| a != b)
                .map(|(a, b)| format!("{a:?}\nvs\n{b:?}"));
            panic!(
                "{name}: {} rows on the server, {} in the mirror; first difference: {first:?}",
                want.len(),
                got.len()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn drop_slot(name: &str) {
    sql(&format!(
        "select pg_drop_replication_slot('{name}') from pg_replication_slots where slot_name = '{name}'"
    ));
    sql(&format!("drop publication if exists {name}"));
}

#[test]
#[ignore]
fn every_kind_of_change_arrives() {
    let t = "fenec_follow_kinds";
    table(t, 200);
    let f = Follower::start(t, Arc::new(RwLock::new(Database::new())));
    converge(&f.db, t);
    for q in [
        format!("insert into {t} values (1001, 'new', 1, array['x'], null)"),
        // Only the title: the vector is TOASTed and does not come along.
        format!("update {t} set title = 'retitled' where id <= 20"),
        format!("update {t} set tags = null, n = n * 2 where id between 21 and 30"),
        // A key that changes: the document moves.
        format!("update {t} set id = 5000 where id = 40"),
        format!("delete from {t} where id between 100 and 150"),
        // Several changes to one row in one transaction.
        format!(
            "begin; insert into {t} values (2000, 'a', 0, null, null); \
             update {t} set title = 'b' where id = 2000; \
             update {t} set title = 'c', n = 3 where id = 2000; commit"
        ),
        // Inserted and deleted before the commit: never there at all.
        format!("begin; insert into {t} values (3000, 'gone', 0, null, null); delete from {t} where id = 3000; commit"),
    ] {
        sql(&q);
    }
    converge(&f.db, t);
    sql(&format!("truncate {t}"));
    sql(&format!(
        "insert into {t} values (1, 'after the truncate', 1, null, null)"
    ));
    converge(&f.db, t);
    f.stop();
    drop_slot(t);
}

#[test]
#[ignore]
fn a_broken_stream_loses_nothing() {
    let t = "fenec_follow_broken";
    table(t, 100);
    let f = Follower::start(t, Arc::new(RwLock::new(Database::new())));
    converge(&f.db, t);
    for round in 0..3 {
        for i in 0..20 {
            sql(&format!(
                "update {t} set n = {} where id = {}",
                round * 100 + i,
                1 + (round * 20 + i) % 100
            ));
        }
        // The server ends the follower's connection.
        sql("select pg_terminate_backend(pid) from pg_stat_replication \
             where application_name = 'fenec-follow'");
        sql(&format!(
            "insert into {t} values ({}, 'during', 0, null, null)",
            500 + round
        ));
    }
    converge(&f.db, t);
    assert!(f.saw("Reconnecting"), "{:?}", f.events.lock().unwrap());
    f.stop();
    drop_slot(t);
}

#[test]
#[ignore]
fn a_stopped_follower_resumes_without_a_copy() {
    let t = "fenec_follow_resume";
    table(t, 50);
    let db = Arc::new(RwLock::new(Database::new()));
    let f = Follower::start(t, Arc::clone(&db));
    converge(&db, t);
    let first = f.stop();
    assert!(first.iter().any(|e| e.contains("Copied")), "{first:?}");

    // Written while nothing follows: the slot holds it.
    sql(&format!(
        "update {t} set title = 'while stopped' where id <= 10"
    ));
    sql(&format!("delete from {t} where id > 40"));

    let f = Follower::start(t, Arc::clone(&db));
    converge(&db, t);
    let second = f.stop();
    assert!(!second.iter().any(|e| e.contains("Copying")), "{second:?}");
    drop_slot(t);
}

#[test]
#[ignore]
fn a_copy_cut_short_is_made_again() {
    let t = "fenec_follow_cut";
    table(t, 30);
    let db = Arc::new(RwLock::new(Database::new()));
    let f = Follower::start(t, Arc::clone(&db));
    converge(&db, t);
    f.stop();

    // As if the process had died halfway through the copy: most of the
    // rows missing, and `_follow` saying the copy never finished.
    {
        let mut g = db.write().unwrap();
        g.execute(&fenec_ql::parse_one(&format!("del {t} where id > 5")).unwrap())
            .unwrap();
        g.execute(
            &fenec_ql::parse_one(&format!(
                "set _follow {{copied: false}} where collection = \"{t}\""
            ))
            .unwrap(),
        )
        .unwrap();
    }
    let f = Follower::start(t, Arc::clone(&db));
    converge(&db, t);
    let events = f.stop();
    assert!(
        events.iter().any(|e| e.contains("did not finish")),
        "{events:?}"
    );

    // A collection a follower did not copy is not the follower's to replace.
    let other = Arc::new(RwLock::new(Database::new()));
    other
        .write()
        .unwrap()
        .execute(&fenec_ql::parse_one(&format!("create collection {t} (x int)")).unwrap())
        .unwrap();
    let mut f = Follower::start(t, Arc::clone(&other));
    let result = f.thread.take().unwrap().join().unwrap();
    assert!(
        result
            .as_ref()
            .is_err_and(|e| e.to_string().contains("not copied by a follower")),
        "{result:?}"
    );
    drop_slot(t);
}
