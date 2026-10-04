//! The sync log (`synclog.rs`): what a sync made durable through it is the
//! file's again after the machine lost it, mapped or read; and a log is
//! applied to its own file alone, once.
//!
//! A crash of the machine is the process's end with no close
//! (`std::mem::forget`, which leaves the log as a crash would) and the file
//! cut back to where its last fsync of its own left it: the start of the
//! log's generation, which the log's header names.

#![cfg(all(feature = "std-fs", unix))]

use fenec_core::fs;
use fenec_core::prelude::*;
use std::path::{Path, PathBuf};

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

fn log_of(path: &Path) -> PathBuf {
    fs::beside(path, "sync")
}

/// Where the file stood synced when the log's generation began, from its
/// header (`synclog::Header`): `None` with no log, or none in force.
fn base(path: &Path) -> Option<u64> {
    let log = std::fs::read(log_of(path)).ok()?;
    (log.get(..8)? == b"FENECSYN").then(|| u64::from_le_bytes(log[24..32].try_into().unwrap()))
}

/// The process ends with no close, and the machine with it: the file keeps
/// what its own fsync covered, and `into` bytes of what came after.
fn power_loss(db: Database, path: &Path, into: u64) -> u64 {
    std::mem::forget(db);
    let at = base(path).expect("a log in force");
    let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_len(at + into).unwrap();
    at
}

struct Dir(PathBuf);
impl Dir {
    fn new(tag: &str) -> Dir {
        fs::keep_sync_log(true);
        let d = std::env::temp_dir().join(format!("fenecdb-synclog-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Dir(d)
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        fs::keep_sync_log(false);
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The writes a sync made durable through the log are the file's again
/// after the machine lost them from it -- cut off, or cut through a
/// record -- whether the file is mapped or read; a write made after the
/// last sync is not. A clean close syncs the file and removes the log, and
/// a compact's rename leaves none beside the new file.
#[test]
fn the_sync_log_gives_back_what_the_machine_lost() {
    let dir = Dir::new("lost");
    let path = dir.0.join("db.fenec");
    for (mapped, cut_into) in [(true, 0), (false, 0), (true, 7), (false, 3)] {
        let _ = std::fs::remove_file(&path);
        let mut db = fs::open_with(&path, mapped, Box::new(Ok)).unwrap();
        run(&mut db, "create collection t (n int)");
        // The first sync fsyncs the file and begins the log there.
        durable(&mut db);
        for i in 0..50 {
            run(&mut db, &format!("put t {{n: {i}}}"));
            durable(&mut db);
        }
        let whole = std::fs::read(&path).unwrap();
        run(&mut db, "put t {n: 99}");
        db.write_out().unwrap();
        let at = power_loss(db, &path, cut_into);
        assert!((whole.len() as u64) > at);
        let mut db = fs::open_with(&path, mapped, Box::new(Ok)).unwrap();
        assert_eq!(count(&mut db), 50, "mapped {mapped}, cut {cut_into}");
        assert_eq!(std::fs::read(&path).unwrap(), whole);
        // Written on, the log begins again where the file stands.
        run(&mut db, "put t {n: 100}");
        durable(&mut db);
        run(&mut db, "put t {n: 101}");
        durable(&mut db);
        drop(db);
        assert!(!log_of(&path).exists(), "a clean close removes the log");
        let mut db = fs::open_with(&path, mapped, Box::new(Ok)).unwrap();
        assert_eq!(count(&mut db), 52);
        // A compact renames another file over this one.
        durable(&mut db);
        run(&mut db, "put t {n: 102}");
        durable(&mut db);
        assert!(log_of(&path).exists());
        run(&mut db, "compact t");
        assert!(!log_of(&path).exists(), "the rename leaves no log beside");
        run(&mut db, "put t {n: 103}");
        durable(&mut db);
        run(&mut db, "put t {n: 104}");
        durable(&mut db);
        power_loss(db, &path, 0);
        let mut db = fs::open_with(&path, mapped, Box::new(Ok)).unwrap();
        assert_eq!(count(&mut db), 55);
        drop(db);
    }
}

/// A log full of entries has the file fsynced and begins a generation from
/// there; a machine lost after it gets back what the new one holds.
#[test]
fn a_full_log_begins_again_from_the_file() {
    let dir = Dir::new("full");
    let path = dir.0.join("db.fenec");
    let mut db = fs::open(&path).unwrap();
    run(&mut db, "create collection t (n int, s text)");
    durable(&mut db);
    run(&mut db, "put t {n: -1}");
    durable(&mut db);
    let first = base(&path).unwrap();
    let text = "x".repeat(1000);
    for i in 0..400 {
        run(&mut db, &format!("put t {{n: {i}, s: \"{text}\"}}"));
        durable(&mut db);
    }
    let later = base(&path).unwrap();
    assert!(
        later > first,
        "the log filled and began again: {first} -> {later}"
    );
    let whole = std::fs::read(&path).unwrap();
    power_loss(db, &path, 100);
    let mut db = fs::open(&path).unwrap();
    assert_eq!(count(&mut db), 401);
    assert_eq!(std::fs::read(&path).unwrap(), whole);
}

/// A sync holding more than an entry takes fsyncs the file itself, and the
/// log begins after it.
#[test]
fn a_sync_past_an_entry_fsyncs_the_file() {
    let dir = Dir::new("big");
    let path = dir.0.join("db.fenec");
    let mut db = fs::open(&path).unwrap();
    run(&mut db, "create collection t (n int, s text)");
    durable(&mut db);
    run(&mut db, "put t {n: -1}");
    durable(&mut db);
    let first = base(&path).unwrap();
    let text = "y".repeat(1000);
    let rows: Vec<String> = (0..100)
        .map(|i| format!("{{n: {i}, s: \"{text}\"}}"))
        .collect();
    run(&mut db, &format!("put t [{}]", rows.join(", ")));
    durable(&mut db);
    run(&mut db, "put t {n: 100}");
    durable(&mut db);
    // The generation the next entry wrote begins past the 100 KB.
    let after = base(&path).unwrap();
    assert!(
        after >= first + 100_000,
        "the file was synced: {first} -> {after}"
    );
    power_loss(db, &path, 0);
    let mut db = fs::open(&path).unwrap();
    assert_eq!(count(&mut db), 102);
}

/// A header a crash cut short, or a log gone, applies nothing and loses
/// nothing the file's own fsync covered: what reached only the log was not
/// acknowledged while its header was being written.
#[test]
fn a_torn_or_lost_header_applies_nothing() {
    let dir = Dir::new("torn");
    for lost in [false, true] {
        let path = dir.0.join(format!("db-{lost}.fenec"));
        let mut db = fs::open(&path).unwrap();
        run(&mut db, "create collection t (n int)");
        run(&mut db, "put t {n: 0}");
        durable(&mut db);
        run(&mut db, "put t {n: 1}");
        durable(&mut db);
        let at = base(&path).unwrap();
        std::mem::forget(db);
        if lost {
            std::fs::remove_file(log_of(&path)).unwrap();
        } else {
            let mut log = std::fs::read(log_of(&path)).unwrap();
            log[20] ^= 0x40;
            std::fs::write(log_of(&path), log).unwrap();
        }
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(at).unwrap();
        let mut db = fs::open(&path).unwrap();
        assert_eq!(count(&mut db), 1, "lost {lost}");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), at);
    }
}

/// A log is applied to the file it was written beside, not to another
/// renamed into its place -- here a copy of the same bytes, which the hash
/// of the bytes before the generation's start cannot tell apart.
#[test]
fn a_log_is_not_applied_to_a_file_renamed_in() {
    let dir = Dir::new("renamed");
    let path = dir.0.join("db.fenec");
    let mut db = fs::open(&path).unwrap();
    run(&mut db, "create collection t (n int)");
    durable(&mut db);
    for i in 0..5 {
        run(&mut db, &format!("put t {{n: {i}}}"));
        durable(&mut db);
    }
    let at = power_loss(db, &path, 0);
    let copy = dir.0.join("copy.fenec");
    std::fs::copy(&path, &copy).unwrap();
    std::fs::rename(&copy, &path).unwrap();
    let mut db = fs::open(&path).unwrap();
    assert_eq!(count(&mut db), 0);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), at);
    drop(db);
    // What a restore or an import does before its rename removes it.
    std::fs::write(log_of(&path), b"a stale log").unwrap();
    fs::forget_sync_log(&path).unwrap();
    assert!(!log_of(&path).exists());
}

/// A log applied once is spent: a file cut below its end after the open --
/// a binary from before the log, a cut of a torn record -- is not written
/// over again at the next.
#[test]
fn a_log_is_applied_once() {
    let dir = Dir::new("once");
    let path = dir.0.join("db.fenec");
    let mut db = fs::open(&path).unwrap();
    run(&mut db, "create collection t (n int)");
    durable(&mut db);
    for i in 0..5 {
        run(&mut db, &format!("put t {{n: {i}}}"));
        durable(&mut db);
    }
    let at = power_loss(db, &path, 0);
    let db = fs::open(&path).unwrap();
    std::mem::forget(db);
    assert_eq!(base(&path), None, "the header is wiped once applied");
    let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    f.set_len(at).unwrap();
    let mut db = fs::open(&path).unwrap();
    assert_eq!(count(&mut db), 0);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), at);
}

/// `x.db` and `x.fenec` keep logs of their own: with the extension
/// replaced, both were `x.fenec.sync`, each writing over the other.
#[test]
fn two_files_of_one_stem_keep_two_logs() {
    let dir = Dir::new("stem");
    let a = dir.0.join("x.db");
    let b = dir.0.join("x.fenec");
    let mut da = fs::open(&a).unwrap();
    let mut db = fs::open(&b).unwrap();
    for d in [&mut da, &mut db] {
        run(d, "create collection t (n int)");
        durable(d);
    }
    for i in 0..10 {
        run(&mut da, &format!("put t {{n: {i}}}"));
        durable(&mut da);
        run(&mut db, &format!("put t [{{n: {i}}}, {{n: {}}}]", i + 100));
        durable(&mut db);
    }
    assert_ne!(log_of(&a), log_of(&b));
    power_loss(da, &a, 0);
    power_loss(db, &b, 0);
    let (mut da, mut db) = (fs::open(&a).unwrap(), fs::open(&b).unwrap());
    assert_eq!((count(&mut da), count(&mut db)), (10, 20));
}

/// A log that cannot be made -- here a directory stands at its name, where
/// a read-only directory would not stop a process run as root -- leaves
/// every sync to the file's own fsync, and the writes go on.
#[test]
fn a_log_that_cannot_be_made_leaves_the_file_to_its_fsync() {
    let dir = Dir::new("unmade");
    let path = dir.0.join("db.fenec");
    let mut db = fs::open(&path).unwrap();
    run(&mut db, "create collection t (n int)");
    drop(db);
    std::fs::create_dir(log_of(&path)).unwrap();
    let mut db = fs::open(&path).unwrap();
    for i in 0..5 {
        run(&mut db, &format!("put t {{n: {i}}}"));
        durable(&mut db);
    }
    run(&mut db, "compact t");
    run(&mut db, "put t {n: 5}");
    durable(&mut db);
    assert!(log_of(&path).is_dir());
    drop(db);
    let mut db = fs::open(&path).unwrap();
    assert_eq!(count(&mut db), 6);
}
