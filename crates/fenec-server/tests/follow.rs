//! `fenec-server --follow` against a live PostgreSQL (`make import-test`,
//! which starts one in Docker): a table's copy and its changes served over
//! HTTP as they commit, a client's write to the mirror refused, and a
//! server stopped -- by a signal, or killed outright -- started again over
//! its file without losing a row.
//!
//!     cargo test -p fenec-server --test all follow:: -- --ignored

#![cfg(unix)]

use crate::support::{count, Server};
use fenec_wire::client::{Client, Url};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const URL: &str = "postgres://postgres:fenec@127.0.0.1:55432/fenecbench";

fn source_text() -> String {
    std::env::var("FENEC_TEST_PG_URL").unwrap_or_else(|_| URL.into())
}

fn pg() -> Client {
    Client::connect(&Url::parse(&source_text()).unwrap())
        .expect("no PostgreSQL to follow (make pgvector-up)")
}

/// A table of `rows` rows, with no slot or publication left from before:
/// the follower names both `fenec_<collection>`.
fn table(c: &mut Client, t: &str, rows: u64) {
    for q in [
        format!("select pg_drop_replication_slot('fenec_{t}') from pg_replication_slots where slot_name = 'fenec_{t}'"),
        format!("drop publication if exists fenec_{t}"),
        format!("drop table if exists {t}"),
        format!("create table {t} (id bigint primary key, title text)"),
        format!("insert into {t} select g, 'row ' || g from generate_series(1, {rows}) g"),
    ] {
        c.query(&q).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
}

fn forget(c: &mut Client, t: &str) {
    let _ = c.query(&format!("select pg_drop_replication_slot('fenec_{t}')"));
    let _ = c.query(&format!("drop publication if exists fenec_{t}"));
    let _ = c.query(&format!("drop table if exists {t}"));
}

fn rows(s: &Server, t: &str) -> u64 {
    count(&s.http().run(&format!("get {t} count"))) as u64
}

/// Waits until a row of the mirror has `title`.
fn shows(s: &Server, t: &str, title: &str) {
    let q = format!("get {t} where title = \"{title}\" count");
    let deadline = Instant::now() + Duration::from_secs(30);
    while count(&s.http().run(&q)) != 1 {
        assert!(
            Instant::now() < deadline,
            "no row of {t} has title `{title}`"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Waits until the mirror holds `n` rows.
fn holds(s: &Server, t: &str, n: u64) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while rows(s, t) != n {
        assert!(
            Instant::now() < deadline,
            "the mirror of {t} stayed at {}",
            rows(s, t)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn tmp(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("fenec-server-follow-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("mirror.fenec")
}

/// Starts the server over `file`, following `t`, and waits until it streams.
fn start(file: &Path, t: &str) -> Server {
    let source = source_text();
    let s = crate::support::start(&[
        "--file",
        file.to_str().unwrap(),
        "--follow",
        &source,
        "--follow-table",
        t,
    ]);
    assert!(
        s.logged("streaming its changes", Duration::from_secs(60)),
        "fenec-server never streamed:\n{}",
        s.log.lock().unwrap()
    );
    s
}

#[test]
#[ignore = "needs a PostgreSQL to follow: make import-test"]
fn a_followed_table_is_served_and_takes_no_other_write() {
    let t = "fenec_server_follow_served";
    let mut c = pg();
    table(&mut c, t, 50);
    let file = tmp("served");
    let s = start(&file, t);
    assert_eq!(rows(&s, t), 50);

    // A change committed there is served here: each in its own
    // transaction, and applied in their order.
    c.query(&format!("insert into {t} values (51, 'new')"))
        .unwrap();
    c.query(&format!("update {t} set title = 'changed' where id = 7"))
        .unwrap();
    c.query(&format!("delete from {t} where id = 8")).unwrap();
    c.query(&format!("insert into {t} values (52, 'last')"))
        .unwrap();
    shows(&s, t, "last");
    assert_eq!(rows(&s, t), 51);
    let body = s.http().run(&format!("get {t} where id = 7"));
    assert!(body.contains("changed"), "{body}");

    // The mirror is the follower's to write; the rest of the file is not.
    let (status, body) = s
        .http()
        .query(&format!("put {t} {{id: 99, title: \"mine\"}}"))
        .unwrap_err();
    assert_eq!(status, 403, "{body}");
    assert!(body.contains("mirrors a PostgreSQL table"), "{body}");
    let (status, _) = s
        .http()
        .query(&format!("del {t} where id = 1"))
        .unwrap_err();
    assert_eq!(status, 403);
    s.http().run("create collection notes (n int)");
    s.http().run("put notes {n: 1}");
    assert_eq!(rows(&s, t), 51);

    drop(s);
    forget(&mut c, t);
}

#[test]
#[ignore = "needs a PostgreSQL to follow: make import-test"]
fn a_server_stopped_or_killed_goes_on_without_losing_a_row() {
    let t = "fenec_server_follow_restart";
    let mut c = pg();
    table(&mut c, t, 20);
    let file = tmp("restart");

    // Stopped by a signal: its last changes on disk and confirmed, and the
    // ones committed while it was down taken in when it is back.
    let s = start(&file, t);
    holds(&s, t, 20);
    let _ = s.terminate();
    c.query(&format!(
        "insert into {t} select g, 'down ' || g from generate_series(21, 40) g"
    ))
    .unwrap();
    let s = start(&file, t);
    holds(&s, t, 40);

    // Killed outright while the table is written to: what it applied and
    // did not confirm comes again, and changes nothing.
    let written = Arc::new(AtomicU64::new(40));
    let done = Arc::new(AtomicBool::new(false));
    let writer = {
        let (written, done) = (Arc::clone(&written), Arc::clone(&done));
        std::thread::spawn(move || {
            let mut c = pg();
            while !done.load(Ordering::Relaxed) {
                let id = written.load(Ordering::Relaxed) + 1;
                c.query(&format!("insert into {t} values ({id}, 'burst')"))
                    .unwrap();
                written.store(id, Ordering::Relaxed);
            }
        })
    };
    std::thread::sleep(Duration::from_millis(300));
    drop(s);
    std::thread::sleep(Duration::from_millis(300));
    done.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    let s = start(&file, t);
    holds(&s, t, written.load(Ordering::Relaxed));

    drop(s);
    forget(&mut c, t);
}
