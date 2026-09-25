//! Backups and restoring to a moment, against a primary serving in this
//! process: what a restore to a time, a change or the end holds, next to
//! what the primary held then.

use fenec_core::prelude::*;
use fenec_http::archive::{self, Archive, Target};
use fenec_http::replication::{self, fresh_id, Replication, Upstream};
use fenec_http::{Config, Server};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TOKEN: &str = "t";

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fenec-archive-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Primary {
    db: Arc<RwLock<Database>>,
    url: String,
}

fn primary(file: &Path, buffer: usize) -> Primary {
    let (mut db, feed) = replication::open(file.to_str().unwrap(), buffer).unwrap();
    db.fork(fresh_id()).unwrap();
    let db = Arc::new(RwLock::new(db));
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        sync_on_write: true,
        ..Config::default()
    };
    let server = Server::new(Arc::clone(&db), cfg).with_replication(Replication::new(
        TOKEN.into(),
        Some(feed),
        None,
    ));
    let listener = server.bind().unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    Primary { db, url }
}

impl Primary {
    /// A write as a client makes one: executed, then fsynced -- which is
    /// what sends it to the archive.
    fn exec(&self, sql: &str) {
        let durable = {
            let mut g = self.db.write().unwrap();
            g.execute(&fenec_ql::parse_one(sql).unwrap()).unwrap();
            g.flush().unwrap()
        };
        if let Some(d) = durable {
            d().unwrap();
        }
    }

    fn seq(&self) -> u64 {
        self.db.read().unwrap().change_seq()
    }
}

fn rows(db: &Database, sql: &str) -> Vec<(u64, Vec<Value>)> {
    let r = db.query(&fenec_ql::parse_one(sql).unwrap(), &[]).unwrap();
    r.rows()
        .unwrap()
        .rows
        .iter()
        .map(|r| (r.id, r.values.clone()))
        .collect()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// An archiver on its own thread, and the flag that stops it.
struct Archiver {
    stop: Arc<AtomicBool>,
    done: std::thread::JoinHandle<std::io::Result<()>>,
}

fn archiver(path: &Path, url: &str) -> Archiver {
    let stop = Arc::new(AtomicBool::new(false));
    let (flag, path, url) = (Arc::clone(&stop), path.to_path_buf(), url.to_string());
    let done = std::thread::spawn(move || {
        let upstream = Upstream::new(&url, TOKEN.into()).unwrap();
        Archive::new(&path)?.follow(&upstream, &flag, &|_| {})
    });
    Archiver { stop, done }
}

impl Archiver {
    fn finish(self) {
        self.stop.store(true, Ordering::SeqCst);
        self.done.join().unwrap().unwrap();
    }
}

/// Waits until a restore to the end reaches `seq`.
fn archived(arch: &Path, seq: u64) {
    let deadline = Instant::now() + Duration::from_secs(20);
    let a = Archive::new(arch).unwrap();
    let probe = arch.join("probe.fenec");
    loop {
        if let Ok(r) = a.restore(&probe, Target::End) {
            if r.seq >= seq {
                return;
            }
        }
        assert!(Instant::now() < deadline, "the archive did not reach {seq}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn restored(arch: &Path, out: &Path, to: Target) -> (Database, archive::Restored) {
    let r = Archive::new(arch).unwrap().restore(out, to).unwrap();
    (fenec_core::fs::open(out).unwrap(), r)
}

#[test]
fn a_restore_holds_what_the_primary_held_at_that_moment() {
    let d = dir("moment");
    let p = primary(&d.join("p.fenec"), replication::DEFAULT_BUFFER);
    p.exec("create collection notes (title text @text, n int @hash)");
    let arch = d.join("archive");
    let a = archiver(&arch, &p.url);
    for i in 0..20 {
        p.exec(&format!("put notes {{title: \"early {i}\", n: {i}}}"));
    }
    let early = p.seq();
    let early_rows = rows(&p.db.read().unwrap(), "get notes");
    std::thread::sleep(Duration::from_millis(30));
    let moment = now_ms();
    std::thread::sleep(Duration::from_millis(30));

    // The mistake to undo, and a compact that folds it into an image: the
    // archive holds the writes as they were made all the same.
    p.exec("del notes where n < 10");
    p.exec("put notes {title: \"late\", n: 99}");
    p.exec("compact");
    for i in 0..5 {
        p.exec(&format!(
            "put notes {{title: \"later {i}\", n: {}}}",
            100 + i
        ));
    }
    archived(&arch, p.seq());
    a.finish();

    // To the moment: the twenty notes, nothing after.
    let (db, r) = restored(&arch, &d.join("moment.fenec"), Target::Time(moment));
    assert_eq!(r.seq, early);
    assert!(r.time.unwrap() <= moment);
    assert_eq!(rows(&db, "get notes"), early_rows);
    assert_eq!(
        rows(&db, r#"get notes select id match title "early""#).len(),
        20
    );
    // Forked: this is a history of its own from here.
    let restored_history = db.history().clone();
    assert!(!restored_history.following);
    assert_ne!(
        restored_history.current(),
        p.db.read().unwrap().history().current()
    );

    // To a change: each document a statement writes is a change of its
    // own, but the statement's writes land together -- the primary never
    // held nineteen notes. Its first change is the moment before it, its
    // tenth all ten gone.
    let (db, r) = restored(&arch, &d.join("change.fenec"), Target::Change(early + 1));
    assert_eq!(r.seq, early);
    assert_eq!(rows(&db, "get notes").len(), 20);
    let (db, r) = restored(&arch, &d.join("change.fenec"), Target::Change(early + 10));
    assert_eq!(r.seq, early + 10);
    assert_eq!(rows(&db, "get notes").len(), 10);

    // To the end: what the primary holds now.
    let (db, _) = restored(&arch, &d.join("end.fenec"), Target::End);
    assert_eq!(
        rows(&db, "get notes"),
        rows(&p.db.read().unwrap(), "get notes")
    );
    assert_eq!(db.change_seq(), p.seq());

    // A restored file takes writes, as a database of its own.
    drop(db);
    let mut db = fenec_core::fs::open(d.join("end.fenec")).unwrap();
    db.execute(&fenec_ql::parse_one("put notes {title: \"after\"}").unwrap())
        .unwrap();
}

#[test]
fn an_archiver_stopped_goes_on_and_one_behind_takes_an_image() {
    let d = dir("resume");
    // A buffer this small keeps only the last few writes.
    let p = primary(&d.join("p.fenec"), 512);
    p.exec("create collection c (x int)");
    let arch = d.join("archive");
    let a = archiver(&arch, &p.url);
    for i in 0..10 {
        p.exec(&format!("put c {{x: {i}}}"));
    }
    archived(&arch, p.seq());
    a.finish();
    let stopped_at = p.seq();
    let before = rows(&p.db.read().unwrap(), "get c");

    // Stopped, it misses more than the primary keeps: the next run takes an
    // image and goes on from there. A restore to the point it was stopped
    // at still starts from the first image.
    for i in 10..200 {
        p.exec(&format!("put c {{x: {i}}}"));
    }
    let a = archiver(&arch, &p.url);
    archived(&arch, p.seq());
    for i in 200..210 {
        p.exec(&format!("put c {{x: {i}}}"));
    }
    archived(&arch, p.seq());
    a.finish();
    let images = std::fs::read_dir(&arch)
        .unwrap()
        .filter(|e| {
            let n = e.as_ref().unwrap().file_name();
            n.to_string_lossy().starts_with("image-")
        })
        .count();
    assert_eq!(images, 2);

    let (db, r) = restored(&arch, &d.join("then.fenec"), Target::Change(stopped_at));
    assert_eq!(r.seq, stopped_at);
    assert_eq!(rows(&db, "get c"), before);
    let (db, _) = restored(&arch, &d.join("end.fenec"), Target::End);
    assert_eq!(rows(&db, "get c"), rows(&p.db.read().unwrap(), "get c"));

    // Between the two images the archive holds no write: a restore there
    // says so rather than inventing one.
    let err = Archive::new(&arch)
        .unwrap()
        .restore(&d.join("gap.fenec"), Target::Change(stopped_at + 5))
        .unwrap_err();
    assert!(err.to_string().contains("lacks change"), "{err}");
}

#[test]
fn a_backup_is_a_database_of_its_own() {
    let d = dir("backup");
    let p = primary(&d.join("p.fenec"), replication::DEFAULT_BUFFER);
    p.exec("create collection c (x int, e vector<3> @hnsw(cosine))");
    for i in 0..50 {
        p.exec(&format!("put c {{x: {i}, e: [{i}.0, 1.0, 0.0]}}"));
    }
    let upstream = Upstream::new(&p.url, TOKEN.into()).unwrap();
    let file = d.join("backup.fenec");
    let seq = archive::backup(&upstream, &file).unwrap();
    assert_eq!(seq, p.seq());

    let mut db = fenec_core::fs::open(&file).unwrap();
    assert_eq!(rows(&db, "get c"), rows(&p.db.read().unwrap(), "get c"));
    assert_eq!(db.change_seq(), seq);
    // Forked where it was taken, and taking writes.
    assert_eq!(db.history().lineage.last().unwrap().1, seq);
    db.execute(&fenec_ql::parse_one("put c {x: 1000}").unwrap())
        .unwrap();

    // Into an archive, it is a base image a restore starts from.
    let arch = d.join("archive");
    std::fs::create_dir_all(&arch).unwrap();
    archive::backup(&upstream, &arch).unwrap();
    let (db, r) = restored(&arch, &d.join("from-image.fenec"), Target::End);
    assert_eq!((r.seq, r.image), (seq, seq));
    assert_eq!(rows(&db, "get c"), rows(&p.db.read().unwrap(), "get c"));
}

/// A block's writes are one record in the archive, as on the primary: a
/// restore to the end holds them all, and one to a change inside the block
/// stops before it -- the primary never stood there, the block landing
/// whole.
#[test]
fn a_block_is_archived_and_restored_whole() {
    let d = dir("block");
    let p = primary(&d.join("primary.fenec"), 1 << 20);
    p.exec("create collection a (n int)");
    p.exec("create collection b (n int)");
    p.exec("put a {n: 1}");
    let arch = d.join("arch");
    let a = archiver(&arch, &p.url);
    // The archive's image first: the block has to come after it.
    let before = p.seq();
    archived(&arch, before);
    let durable = {
        let mut g = p.db.write().unwrap();
        let s: Vec<Statement> = ["put a {n: 2}", "put b {n: 3}", "put a [{n: 4}, {n: 5}]"]
            .iter()
            .map(|q| fenec_ql::parse_one(q).unwrap())
            .collect();
        let block: Vec<(&Statement, &[Value])> = s.iter().map(|s| (s, &[][..])).collect();
        g.execute_block(&block).unwrap();
        g.flush().unwrap()
    };
    if let Some(d) = durable {
        d().unwrap();
    }
    p.exec("put b {n: 6}");
    let end = p.seq();
    assert_eq!(end, before + 5);
    archived(&arch, end);
    a.finish();

    let ns = |db: &Database, c: &str| -> Vec<Value> {
        rows(db, &format!("get {c} select n order n"))
            .into_iter()
            .map(|(_, v)| v[0].clone())
            .collect()
    };
    let (db, r) = restored(&arch, &d.join("end.fenec"), Target::End);
    assert_eq!(r.seq, end);
    assert_eq!(ns(&db, "a"), [1, 2, 4, 5].map(Value::Int));
    assert_eq!(ns(&db, "b"), [3, 6].map(Value::Int));
    // Inside the block: before it.
    let (db, r) = restored(&arch, &d.join("mid.fenec"), Target::Change(before + 2));
    assert_eq!(r.seq, before);
    assert_eq!(ns(&db, "a"), [Value::Int(1)]);
    assert!(ns(&db, "b").is_empty());
    // At its last write: the block whole.
    let (db, r) = restored(&arch, &d.join("whole.fenec"), Target::Change(before + 4));
    assert_eq!(r.seq, before + 4);
    assert_eq!(ns(&db, "a"), [1, 2, 4, 5].map(Value::Int));
    assert_eq!(ns(&db, "b"), [Value::Int(3)]);
}
