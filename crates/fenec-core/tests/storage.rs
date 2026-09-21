//! A storage error stops writes until the file is reopened.
//!
//! After a failed `fsync` the kernel may already have dropped the dirty pages,
//! so a second `sync` that succeeds proves nothing; after a failed append the
//! memory holds a write the file does not. Either way the only honest answers
//! left are the error, for every write that follows, and the reads, which
//! answer from memory.

use fenec_core::prelude::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Appends succeed until `appends` of them have gone through; `sync` fails
/// once `fail_sync` is set. The counters are shared so a test can arm the
/// failure after the setup statements have run.
struct Flaky {
    appends_left: Arc<AtomicUsize>,
    fail_sync: Arc<AtomicUsize>,
}

impl Sink for Flaky {
    fn append(&mut self, _bytes: &[u8]) -> Result<()> {
        let left = self.appends_left.load(Ordering::SeqCst);
        if left == 0 {
            return Err(Error::Io("no space left on device".into()));
        }
        self.appends_left.store(left - 1, Ordering::SeqCst);
        Ok(())
    }
    fn rewrite(&mut self, _bytes: &[u8]) -> Result<()> {
        Ok(())
    }
    fn sync(&mut self) -> Result<()> {
        if self.fail_sync.load(Ordering::SeqCst) > 0 {
            return Err(Error::Io("input/output error".into()));
        }
        Ok(())
    }
}

fn flaky() -> (Database, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let appends = Arc::new(AtomicUsize::new(usize::MAX));
    let fail_sync = Arc::new(AtomicUsize::new(0));
    let db = Database::with_sink(Box::new(Flaky {
        appends_left: Arc::clone(&appends),
        fail_sync: Arc::clone(&fail_sync),
    }));
    (db, appends, fail_sync)
}

fn exec(db: &mut Database, sql: &str) -> Result<Response> {
    db.execute(&fenec_ql::parse_one(sql).expect("parse"))
}

fn names(db: &Database) -> Vec<String> {
    let stmt = fenec_ql::parse_one("get t select name").expect("parse");
    let rows = db.query(&stmt, &[]).expect("a read after the failure");
    rows.rows()
        .expect("rows")
        .rows
        .iter()
        .map(|r| match &r.values[0] {
            Value::Text(s) => s.clone(),
            other => panic!("unexpected {other:?}"),
        })
        .collect()
}

#[test]
fn a_failed_sync_refuses_every_later_write() {
    let (mut db, _, fail_sync) = flaky();
    exec(&mut db, "create collection t (name text)").unwrap();
    exec(&mut db, "put t {name: \"before\"}").unwrap();
    db.sync().expect("the disk still answers");
    assert!(db.failure().is_none());

    exec(&mut db, "put t {name: \"unsynced\"}").unwrap();
    fail_sync.store(1, Ordering::SeqCst);
    assert!(matches!(db.sync(), Err(Error::Io(_))));
    assert!(db.failure().unwrap().contains("input/output error"));

    // The disk answering again changes nothing: a retry that succeeds after
    // the kernel dropped the pages would be a lie.
    fail_sync.store(0, Ordering::SeqCst);
    let again = db.sync().unwrap_err().to_string();
    assert!(again.contains("refused"), "{again}");
    assert!(db.checkpoint().is_err());

    for sql in [
        "put t {name: \"after\"}",
        "set t {name: \"x\"} where name = \"before\"",
        "del t where name = \"before\"",
        "create collection u (a int)",
        "compact",
    ] {
        match exec(&mut db, sql) {
            Err(Error::Io(m)) => assert!(m.contains("refused"), "{sql}: {m}"),
            other => panic!("{sql}: expected a refusal, got {other:?}"),
        }
    }
    // Reads answer from memory, which is what every client was told.
    assert_eq!(names(&db), vec!["before", "unsynced"]);
}

#[test]
fn a_failed_append_refuses_every_later_write() {
    let (mut db, appends, _) = flaky();
    exec(&mut db, "create collection t (name text)").unwrap();
    exec(&mut db, "put t {name: \"kept\"}").unwrap();

    appends.store(0, Ordering::SeqCst);
    let first = exec(&mut db, "put t {name: \"lost\"}").unwrap_err();
    assert!(first.to_string().contains("no space left"), "{first}");

    // Space coming back does not reopen the door either: the memory already
    // holds a write the file does not.
    appends.store(usize::MAX, Ordering::SeqCst);
    let later = exec(&mut db, "put t {name: \"later\"}").unwrap_err();
    assert!(later.to_string().contains("refused"), "{later}");
    assert!(db.sync().is_err());
    assert!(names(&db).contains(&"kept".to_string()));
}

#[test]
fn a_healthy_sink_never_trips_the_gate() {
    let (mut db, _, _) = flaky();
    exec(&mut db, "create collection t (name text)").unwrap();
    for i in 0..100 {
        exec(&mut db, &format!("put t {{name: \"n{i}\"}}")).unwrap();
        if i % 10 == 0 {
            db.sync().unwrap();
        }
    }
    db.checkpoint().unwrap();
    assert!(db.failure().is_none());
    assert_eq!(names(&db).len(), 100);
}
