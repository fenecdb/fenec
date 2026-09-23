//! Engine-level behaviour of the change feed.

use fenec_core::prelude::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

fn run(db: &mut Database, sql: &str) {
    for stmt in fenec_ql::parse(sql).expect("parse") {
        db.execute(&stmt).expect("execute");
    }
}

fn seeded() -> Database {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection tasks (key text @hash, title text, status text @hash)",
    );
    run(
        &mut db,
        r#"put tasks [
             {key: "a", title: "one",   status: "open"},
             {key: "b", title: "two",   status: "open"},
             {key: "c", title: "three", status: "closed"}
           ]"#,
    );
    db
}

fn batch(db: &Database, since: u64, filter: Option<&str>) -> ChangeBatch {
    let filter = filter.map(|w| {
        let stmt = fenec_ql::parse_one(&format!("get tasks where {w}")).expect("filter");
        match stmt {
            Statement::Select(s) => s.filter.expect("condition"),
            _ => panic!("expected a select"),
        }
    });
    match db
        .changes_since("tasks", since, filter.as_ref(), None, &[])
        .expect("changes")
    {
        Changes::Batch(b) => b,
        Changes::Reseed => panic!("unexpected reseed"),
    }
}

#[test]
fn counter_advances_once_per_document() {
    let mut db = Database::new();
    assert_eq!(db.change_seq(), 0);
    run(&mut db, "create collection t (a int)");
    assert_eq!(db.change_seq(), 1, "ddl is a change as well");
    run(&mut db, "put t [{a: 1}, {a: 2}, {a: 3}]");
    assert_eq!(db.change_seq(), 4);
    run(&mut db, "del t where a = 1");
    assert_eq!(db.change_seq(), 5);
}

#[test]
fn batch_carries_current_state_not_history() {
    let mut db = seeded();
    let cursor = db.change_seq();
    // Three writes to the same document: the subscriber sees one row, the last state.
    run(&mut db, r#"set tasks {title: "x"} where key = "a""#);
    run(&mut db, r#"set tasks {title: "y"} where key = "a""#);
    run(&mut db, r#"set tasks {title: "z"} where key = "a""#);

    let b = batch(&db, cursor, None);
    assert_eq!(b.puts.rows.len(), 1, "three writes collapse into one event");
    assert_eq!(b.dels, Vec::<u64>::new());
    let pos = b.puts.columns.iter().position(|c| c == "title").unwrap();
    assert_eq!(b.puts.rows[0].values[pos], Value::Text("z".into()));
    assert_eq!(b.seq, db.change_seq());
}

#[test]
fn delete_then_reinsert_is_a_put() {
    let mut db = seeded();
    let cursor = db.change_seq();
    run(&mut db, r#"del tasks where key = "a""#);
    run(
        &mut db,
        r#"put tasks {key: "a", title: "new", status: "open"}"#,
    );
    let b = batch(&db, cursor, None);
    // The old id was deleted and a new one added: both are visible.
    assert_eq!(b.puts.rows.len(), 1);
    assert_eq!(b.dels.len(), 1);
}

#[test]
fn row_leaving_the_shape_arrives_as_a_delete() {
    let mut db = seeded();
    let cursor = db.change_seq();
    // For a subscriber following the "open" shape this is a *departure*.
    run(&mut db, r#"set tasks {status: "closed"} where key = "a""#);

    let b = batch(&db, cursor, Some(r#"status = "open""#));
    assert!(
        b.puts.rows.is_empty(),
        "the row no longer matches the shape"
    );
    assert_eq!(b.dels.len(), 1, "leaving the shape shows up as a delete");
}

#[test]
fn row_entering_the_shape_arrives_as_a_put() {
    let mut db = seeded();
    let cursor = db.change_seq();
    run(&mut db, r#"set tasks {status: "open"} where key = "c""#);

    let b = batch(&db, cursor, Some(r#"status = "open""#));
    assert_eq!(b.puts.rows.len(), 1);
    assert!(b.dels.is_empty());
}

#[test]
fn projection_keeps_the_id_on_the_row() {
    let db = seeded();
    let cols = ["title".to_string()];
    let Changes::Batch(b) = db
        .changes_since("tasks", 0, None, Some(&cols), &[])
        .unwrap()
    else {
        panic!("reseed")
    };
    assert_eq!(b.puts.columns, vec!["title".to_string()]);
    // Even when the id is not a column it stays on the row: required to apply a delete.
    assert!(b.puts.rows.iter().all(|r| r.id > 0));
}

#[test]
fn overflow_forces_reseed() {
    let mut db = seeded();
    db.set_change_capacity(2);
    let cursor = db.change_seq();
    run(&mut db, r#"set tasks {title: "1"} where key = "a""#);
    run(&mut db, r#"set tasks {title: "2"} where key = "b""#);
    run(&mut db, r#"set tasks {title: "3"} where key = "c""#);
    assert!(matches!(
        db.changes_since("tasks", cursor, None, None, &[]).unwrap(),
        Changes::Reseed
    ));
    assert!(db.change_horizon() > cursor);
}

#[test]
fn schema_change_is_flagged() {
    let mut db = seeded();
    let cursor = db.change_seq();
    run(&mut db, "create index on tasks (title) @hash");
    let b = batch(&db, cursor, None);
    assert!(b.schema_changed);
}

#[test]
fn counter_survives_snapshot_and_reload() {
    let mut db = seeded();
    let at_checkpoint = db.change_seq();
    let image = db.snapshot();

    // One more write after the checkpoint; the image does not contain it.
    run(&mut db, r#"set tasks {title: "after"} where key = "a""#);
    assert_eq!(db.change_seq(), at_checkpoint + 1);

    let mut fresh = Database::new();
    fresh.load(&image).expect("load");
    assert_eq!(
        fresh.change_seq(),
        at_checkpoint,
        "the counter must come back from the image unchanged"
    );
    // No history: only a cursor exactly at this point gets an empty answer.
    assert!(matches!(
        fresh.changes_since("tasks", at_checkpoint, None, None, &[]),
        Ok(Changes::Batch(_))
    ));
    assert!(matches!(
        fresh.changes_since("tasks", at_checkpoint - 1, None, None, &[]),
        Ok(Changes::Reseed)
    ));
}

#[test]
fn writes_after_a_checkpoint_move_the_counter() {
    // Records appended *after* the REC_SEQ at the end of the image must carry
    // the counter forward: file = checkpoint + tail.
    let mut db = seeded();
    let image = db.snapshot();
    let base = db.change_seq();

    let mut other = Database::new();
    other.load(&image).unwrap();
    run(
        &mut other,
        r#"put tasks {key: "d", title: "four", status: "open"}"#,
    );
    let grown = other.snapshot();

    let mut back = Database::new();
    back.load(&grown).unwrap();
    assert_eq!(back.change_seq(), base + 1);
}

#[test]
fn watcher_fires_once_per_statement() {
    struct Counter(AtomicU64, AtomicU64);
    impl Watcher for Counter {
        fn notify(&self, seq: u64) {
            self.0.fetch_add(1, Ordering::SeqCst);
            self.1.store(seq, Ordering::SeqCst);
        }
    }
    let w = Arc::new(Counter(AtomicU64::new(0), AtomicU64::new(0)));
    let mut db = Database::new();
    db.set_watcher(w.clone());

    run(&mut db, "create collection t (a int)");
    run(&mut db, "put t [{a: 1}, {a: 2}, {a: 3}]");
    assert_eq!(w.0.load(Ordering::SeqCst), 2, "one wake-up per statement");
    assert_eq!(w.1.load(Ordering::SeqCst), db.change_seq());

    // A read does not wake anyone.
    run(&mut db, "get t");
    assert_eq!(w.0.load(Ordering::SeqCst), 2);

    // A `del` that matches nothing does not write either.
    run(&mut db, "del t where a = 99");
    assert_eq!(w.0.load(Ordering::SeqCst), 2);
}

#[test]
fn changed_collections_narrows_to_what_moved() {
    let mut db = seeded();
    run(&mut db, "create collection notes (text text)");
    let cursor = db.change_seq();
    run(&mut db, r#"put notes {text: "x"}"#);
    assert_eq!(
        db.changed_collections_since(cursor),
        Some(vec!["notes".to_string()])
    );
    assert_eq!(db.changed_collections_since(db.change_seq()), Some(vec![]));
}

/// The appends a database makes, kept.
struct Tail(Arc<std::sync::Mutex<Vec<u8>>>);

impl Sink for Tail {
    fn append(&mut self, bytes: &[u8]) -> fenec_core::error::Result<()> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(())
    }
    fn rewrite(&mut self, _bytes: &[u8]) -> fenec_core::error::Result<()> {
        Ok(())
    }
}

#[test]
fn a_truncated_tail_still_loads() {
    // The counter header sits at the **start** of the image. At the end of
    // the file, the tail after it half-written -- fenecdb's normal
    // post-crash state -- would break opening. This test protects that
    // property. (The image itself is renamed into place whole; one cut
    // short is a damaged file, which `persist.rs` holds to refusing.)
    let db = seeded();
    let image = db.snapshot();
    let appended = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut writer = Database::new();
    writer.load(&image).unwrap();
    writer.set_sink(Box::new(Tail(Arc::clone(&appended))));
    run(
        &mut writer,
        r#"put tasks {key: "d", title: "four", status: "open"}"#,
    );
    run(
        &mut writer,
        r#"put tasks {key: "e", title: "five", status: "open"}"#,
    );
    let mut full = image.clone();
    full.extend_from_slice(&appended.lock().unwrap());
    assert_eq!(Database::new().load(&full).unwrap(), full.len());
    for cut in [1usize, 7, 12] {
        let mut trimmed = full.clone();
        trimmed.truncate(full.len() - cut);
        let mut back = Database::new();
        let whole = back
            .load(&trimmed)
            .expect("a truncated tail must not break opening");
        // What it took ends where the record cut short begins.
        assert!(whole < trimmed.len(), "{whole} of {}", trimmed.len());
        assert!(
            whole >= image.len(),
            "{whole} of an image of {}",
            image.len()
        );
        assert_eq!(back.change_seq(), writer.change_seq() - 1);
    }
}

#[test]
fn an_append_only_file_without_a_header_counts_from_zero() {
    // `fs::open` writes only MAGIC into an empty file: in a file that has
    // never seen a checkpoint the counter is counted from the records.
    let db = seeded();
    let expected = db.change_seq();

    let magic = fenec_core::engine::MAGIC;
    let mut raw = Vec::from(&magic[..]);
    let image = db.snapshot();
    // We skip the header and take only the body: identical to an old-format
    // file with no header. The header is MAGIC + the fixed-width counter
    // record (1 + 8 + 8); deriving the offset from MAGIC keeps this test
    // honest if the magic ever changes length again.
    raw.extend_from_slice(&image[magic.len() + 17..]);

    let mut back = Database::new();
    back.load(&raw).expect("headerless file");
    assert_eq!(back.change_seq(), expected);
}
