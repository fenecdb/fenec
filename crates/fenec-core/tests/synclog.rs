//! The sync log (`synclog.rs`): what a sync made durable through it is the
//! file's again after the machine lost it, mapped or read.

#![cfg(all(feature = "std-fs", unix))]

use fenec_core::fs;
use fenec_core::prelude::*;

fn run(db: &mut Database, text: &str) {
    for stmt in fenec_ql::parse(text).unwrap() {
        db.execute(&stmt).unwrap();
    }
}

fn durable(db: &mut Database) {
    if let Some(d) = db.flush().unwrap() {
        d().unwrap();
    }
}

fn count(db: &mut Database) -> usize {
    let stmt = fenec_ql::parse_one("get t").unwrap();
    match db.query(&stmt, &[]).unwrap() {
        Response::Rows(rs) => rs.rows.len(),
        _ => unreachable!(),
    }
}

/// The writes a sync made durable through the log are the file's again
/// after the machine lost them from it -- cut off, or cut through a
/// record -- whether the file is mapped or read; a write made after the
/// last sync is not, and the log is not applied to a file a rewrite put
/// in its place.
#[test]
fn the_sync_log_gives_back_what_the_machine_lost() {
    fs::keep_sync_log(true);
    let dir = std::env::temp_dir().join(format!("fenecdb-fs-log-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("db.fenec");
    for (mapped, cut_into) in [(true, 0), (false, 0), (true, 7), (false, 3)] {
        let _ = std::fs::remove_file(&path);
        let mut db = fs::open_with(&path, mapped, Box::new(Ok)).unwrap();
        run(&mut db, "create collection t (n int)");
        // The first sync fsyncs the file and begins the log there.
        durable(&mut db);
        let synced = std::fs::metadata(&path).unwrap().len();
        for i in 0..50 {
            run(&mut db, &format!("put t {{n: {i}}}"));
            durable(&mut db);
        }
        let whole = std::fs::read(&path).unwrap();
        assert!(whole.len() as u64 > synced);
        run(&mut db, "put t {n: 99}");
        db.write_out().unwrap();
        drop(db);
        // The machine went down: the file holds what an fsync of its
        // own covered, and a part of a record past it.
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(synced + cut_into).unwrap();
        drop(f);
        let mut db = fs::open_with(&path, mapped, Box::new(Ok)).unwrap();
        assert_eq!(count(&mut db), 50, "mapped {mapped}, cut {cut_into}");
        assert_eq!(std::fs::read(&path).unwrap(), whole);
        // Written on, the log begins again where the file stands.
        run(&mut db, "put t {n: 100}");
        durable(&mut db);
        run(&mut db, "put t {n: 101}");
        durable(&mut db);
        drop(db);
        let mut db = fs::open_with(&path, mapped, Box::new(Ok)).unwrap();
        assert_eq!(count(&mut db), 52);
        // A compact renames another file over this one: the log it had
        // is let go of, and none is applied to the new file.
        run(&mut db, "compact t");
        run(&mut db, "put t {n: 102}");
        durable(&mut db);
        run(&mut db, "put t {n: 103}");
        durable(&mut db);
        drop(db);
        let mut db = fs::open_with(&path, mapped, Box::new(Ok)).unwrap();
        assert_eq!(count(&mut db), 54);
    }
    fs::keep_sync_log(false);
    let _ = std::fs::remove_dir_all(&dir);
}
