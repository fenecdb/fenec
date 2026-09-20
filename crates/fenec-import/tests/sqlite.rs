//! Verifies the SQLite reader against real files.
//!
//! The fixtures are produced with the system `sqlite3` binary, so what we
//! read is guaranteed to be the format SQLite itself writes rather than our
//! own assumptions. When the binary is missing the tests are skipped --
//! fenec-import does not need `sqlite3` at runtime, only these tests do.

use std::path::{Path, PathBuf};
use std::process::Command;
use fenec_core::prelude::*;
use fenec_import::sqlite::Reader;
use fenec_core::query::Statement as Stmt;
use fenec_import::{load, map, IdSource, Options, Source};

fn dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("fenecimport-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Produces a fixture. `None` means the system has no `sqlite3`.
fn make(name: &str, sql: &str) -> Option<PathBuf> {
    let path = dir().join(format!("{name}.sqlite"));
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    match Command::new("sqlite3").arg(&path).arg(sql).output() {
        Err(_) => None,
        Ok(o) if !o.status.success() => {
            panic!("sqlite3 failed: {}", String::from_utf8_lossy(&o.stderr))
        }
        Ok(_) => Some(path),
    }
}

/// Sets up the fixture, or skips the test when `sqlite3` is missing.
macro_rules! fixture {
    ($name:expr, $sql:expr) => {
        match make($name, $sql) {
            Some(p) => p,
            None => {
                eprintln!("sqlite3 not found, `{}` skipped", $name);
                return;
            }
        }
    };
}

fn rows(path: &Path, table: &str) -> Vec<Vec<Value>> {
    let mut r = Reader::open(path, table).unwrap();
    let mut out = Vec::new();
    while let Some(row) = r.next_row().unwrap() {
        out.push(row);
    }
    out
}

#[test]
fn every_storage_class_round_trips() {
    let db = fixture!(
        "types",
        "create table t (
           a integer, b real, c text, d blob, e integer, f text
         );
         insert into t values (42, 2.5, 'hello world', x'00ff10', null, null);
         insert into t values (-1, -0.5, '', x'', 7, 'last');"
    );
    let got = rows(&db, "t");
    assert_eq!(got.len(), 2);
    assert_eq!(
        got[0],
        vec![
            Value::Int(42),
            Value::Float(2.5),
            Value::Text("hello world".into()),
            Value::Bytes(vec![0x00, 0xff, 0x10]),
            Value::Null,
            Value::Null,
        ]
    );
    assert_eq!(got[1][0], Value::Int(-1));
    assert_eq!(got[1][1], Value::Float(-0.5));
    assert_eq!(got[1][3], Value::Bytes(vec![]));
}

/// Integers are stored with 1, 2, 3, 4, 6 and 8 byte serial types, all of
/// them signed. Missing one of them silently produces a wrong number.
#[test]
fn integers_of_every_width() {
    let db = fixture!(
        "widths",
        "create table t (v integer);
         insert into t values (0), (1), (127), (-128), (32767), (-32768),
           (8388607), (-8388608), (2147483647), (-2147483648),
           (140737488355327), (-140737488355328),
           (9223372036854775807), (-9223372036854775808);"
    );
    let got: Vec<i64> = rows(&db, "t")
        .into_iter()
        .map(|r| match r[0] {
            Value::Int(n) => n,
            ref other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        got,
        vec![
            0,
            1,
            127,
            -128,
            32767,
            -32768,
            8388607,
            -8388608,
            2147483647,
            -2147483648,
            140737488355327,
            -140737488355328,
            i64::MAX,
            i64::MIN,
        ]
    );
}

/// `INTEGER PRIMARY KEY` is stored as NULL in the record; the real value is
/// the cell's rowid. Without substituting it, every row loses its id.
#[test]
fn rowid_alias_is_substituted() {
    let db = fixture!(
        "rowid",
        "create table t (id integer primary key, name text);
         insert into t values (10, 'ten'), (20, 'twenty');
         insert into t (name) values ('automatic');"
    );
    let got = rows(&db, "t");
    assert_eq!(got[0][0], Value::Int(10));
    assert_eq!(got[1][0], Value::Int(20));
    assert_eq!(got[2][0], Value::Int(21), "an automatic rowid follows 20");
}

/// A payload that does not fit the page spreads over the overflow chain. If
/// the chain is not walked correctly, text is cut off in the middle.
#[test]
fn overflow_chain_is_followed() {
    let db = fixture!(
        "overflow",
        "create table t (long text, huge blob);
         insert into t values (
           replace(hex(zeroblob(20000)), '0', 'x'),
           zeroblob(50000)
         );"
    );
    let got = rows(&db, "t");
    let Value::Text(s) = &got[0][0] else { panic!() };
    assert_eq!(s.len(), 40000, "the overflowing text was truncated");
    assert!(s.chars().all(|c| c == 'x'));
    let Value::Bytes(b) = &got[0][1] else { panic!() };
    assert_eq!(b.len(), 50000);
    assert!(b.iter().all(|&x| x == 0));
}

/// A table that does not fit in one page produces interior nodes; if the
/// walker does not descend through them, most rows stay invisible.
#[test]
fn interior_pages_are_descended() {
    let db = fixture!(
        "large",
        "create table t (id integer primary key, v text);
         insert into t (v) with recursive c(x) as (
           select 1 union all select x + 1 from c where x < 50000
         ) select 'row ' || x from c;"
    );
    let got = rows(&db, "t");
    assert_eq!(got.len(), 50_000);
    // The rowid order must be preserved.
    assert_eq!(got[0][0], Value::Int(1));
    assert_eq!(got[0][1], Value::Text("row 1".into()));
    assert_eq!(got[49_999][0], Value::Int(50_000));
    assert_eq!(got[49_999][1], Value::Text("row 50000".into()));
}

#[test]
fn empty_table_yields_no_rows() {
    let db = fixture!("empty", "create table t (a int, b text);");
    assert!(rows(&db, "t").is_empty());
    let mut r = Reader::open(&db, "t").unwrap();
    assert_eq!(r.columns().unwrap().len(), 2);
}

/// After an `ALTER TABLE ADD COLUMN` the new column is absent from older
/// records entirely; it has to be filled in as NULL.
#[test]
fn columns_added_later_read_as_null() {
    let db = fixture!(
        "alter",
        "create table t (a int);
         insert into t values (1);
         alter table t add column b text;
         insert into t values (2, 'there');"
    );
    let got = rows(&db, "t");
    assert_eq!(got[0], vec![Value::Int(1), Value::Null]);
    assert_eq!(got[1], vec![Value::Int(2), Value::Text("there".into())]);
}

/// Untyped columns are resolved from a value sample.
#[test]
fn untyped_columns_are_inferred_from_values() {
    let db = fixture!(
        "untyped",
        "create table t (a, b, c);
         insert into t values (1, 'text', 1.5);
         insert into t values (2, 'more', 2.5);"
    );
    let mut r = Reader::open(&db, "t").unwrap();
    let cols = r.columns().unwrap();
    assert_eq!(cols[0].ty, Some(DataType::Int));
    assert_eq!(cols[1].ty, Some(DataType::Text));
    assert_eq!(cols[2].ty, Some(DataType::Float));
    assert!(cols[0].note.as_ref().unwrap().contains("no declared type"));
}

#[test]
fn without_rowid_tables_are_rejected_clearly() {
    let db = fixture!(
        "wr",
        "create table t (a text primary key, b int) without rowid;
         insert into t values ('x', 1);"
    );
    let mut r = Reader::open(&db, "t").unwrap();
    let e = r.next_row().unwrap_err().to_string();
    assert!(e.contains("WITHOUT ROWID"), "{e}");
}

/// When an unprocessed WAL exists the main file shows stale data; stopping
/// beats silently returning it.
#[test]
fn pending_wal_is_refused() {
    let db = fixture!("wal", "create table t (a int); insert into t values (1);");
    let wal = format!("{}-wal", db.display());
    std::fs::write(&wal, vec![0u8; 512]).unwrap();
    let e = Reader::open(&db, "t").unwrap_err().to_string();
    assert!(e.contains("wal_checkpoint"), "{e}");
    let _ = std::fs::remove_file(&wal);
    // An empty WAL is not a blocker.
    std::fs::write(&wal, b"").unwrap();
    assert!(Reader::open(&db, "t").is_ok());
    let _ = std::fs::remove_file(&wal);
}

#[test]
fn missing_table_lists_what_exists() {
    let db = fixture!(
        "missing",
        "create table alpha (a int); create table beta (b int);"
    );
    let e = Reader::open(&db, "gamma").unwrap_err().to_string();
    assert!(e.contains("alpha, beta"), "{e}");
}

#[test]
fn not_a_sqlite_file_is_reported() {
    let path = dir().join("junk.sqlite");
    std::fs::write(&path, b"this is not a database").unwrap();
    let e = Reader::open(&path, "t").unwrap_err().to_string();
    assert!(e.contains("not a SQLite file"), "{e}");
}

// ------------------------------------------------------------ end to end

/// fenec-bench writes vectors into SQLite as f32 little-endian BLOBs
/// (`crates/fenec-bench/src/main.rs`, `blob()`). This checks we read the same
/// encoding and takes the load end to end.
#[test]
fn blob_vectors_load_and_index() {
    let db = fixture!(
        "vector",
        "create table docs (id integer primary key, title text, embed blob);
         insert into docs values
           (1, 'one',   x'0000803F0000000000000000'),
           (2, 'two',   x'000000000000803F00000000'),
           (3, 'three', x'00000000000000000000803F');"
    );

    let mut src = Reader::open(&db, "docs").unwrap();
    let mut target = Database::new();
    let mut opts = Options::new("articles");
    opts.vectors.push(("embed".into(), 3));
    opts.indexes.push((
        "embed".into(),
        IndexKind::Vector(VectorIndexSpec::default()),
    ));

    let summary = load::run(&mut src, &mut target, &opts).unwrap();
    assert_eq!(summary.rows, 3);

    let stats = target.stats();
    assert_eq!(stats[0].documents, 3);
    assert_eq!(stats[0].vector_indexes[0].field, "embed");
    assert_eq!(stats[0].vector_indexes[0].dim, 3);

    // The source ids must be preserved and the vectors decoded correctly.
    let Response::Rows(rs) = target
        .execute(&Statement::Select(Select {
            collection: "articles".into(),
            project: Some(vec!["id".into(), "title".into()]),
            near: Some(Near {
                field: "embed".into(),
                vector: Expr::Lit(Value::Vector(vec![0.0, 1.0, 0.0])),
                ef: None,
                exact: true,
            }),
            limit: Some(1),
            ..Default::default()
        }))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(rs.rows[0].values[0], Value::Int(2));
    assert_eq!(rs.rows[0].values[1], Value::Text("two".into()));
}

/// A decimal column must not silently fall back to float; it has to ask for `--cast`.
#[test]
fn decimal_column_demands_cast() {
    let db = fixture!(
        "money",
        "create table t (id integer primary key, price decimal(10,2));
         insert into t values (1, 19.99);"
    );
    let mut src = Reader::open(&db, "t").unwrap();
    let cols = src.columns().unwrap();
    let e = map::plan(&cols, &Options::new("m")).unwrap_err().to_string();
    assert!(e.contains("--cast price="), "{e}");

    // Asked for as a float, it is parsed from the text value.
    let mut src = Reader::open(&db, "t").unwrap();
    let mut o = Options::new("m");
    o.casts.push(("price".into(), DataType::Float));
    let mut target = Database::new();
    load::run(&mut src, &mut target, &o).unwrap();
    assert_eq!(target.stats()[0].documents, 1);
}

/// A large table must load end to end; this also shows that the interior
/// node walk works together with the load pipeline.
#[test]
fn large_table_loads_end_to_end() {
    let db = fixture!(
        "many",
        "create table t (id integer primary key, category text, score int);
         insert into t (category, score) with recursive c(x) as (
           select 1 union all select x + 1 from c where x < 20000
         ) select 'k' || (x % 7), x from c;"
    );
    let mut src = Reader::open(&db, "t").unwrap();
    let mut target = Database::new();
    let mut o = Options::new("t");
    o.id = IdSource::Auto;
    o.indexes.push(("category".into(), IndexKind::Hash));
    let s = load::run(&mut src, &mut target, &o).unwrap();
    assert_eq!(s.rows, 20_000);
    assert_eq!(target.stats()[0].documents, 20_000);
}

/// `--where` works with SQLite too: the filter is FenecQL and is applied
/// before the row is written. Since there is no query engine on the source
/// side, it runs locally.
#[test]
fn where_filter_applies_to_sqlite() {
    let db = fixture!(
        "filter",
        "create table t (id integer primary key, category text, score int);
         insert into t (category, score) values
           ('book', 5), ('book', 50), ('article', 60), ('article', 5);"
    );
    let Stmt::Select(sel) =
        fenec_ql::parse_one(r#"get t where category = "book" and score >= 10"#).unwrap()
    else {
        panic!()
    };

    let mut src = Reader::open(&db, "t").unwrap();
    let mut target = Database::new();
    let mut o = Options::new("m");
    o.filter = sel.filter;
    let s = load::run(&mut src, &mut target, &o).unwrap();
    assert_eq!(s.rows, 1);
    assert_eq!(s.skipped, 3);

    let Response::Rows(rs) = target
        .execute(&Statement::Select(Select {
            collection: "m".into(),
            project: Some(vec!["score".into()]),
            ..Default::default()
        }))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(rs.rows[0].values[0], Value::Int(50));
}

/// Counting only counts the cells without decoding the record body; on a
/// multi-page table it has to descend through the interior nodes too.
#[test]
fn rows_can_be_counted_without_reading_them() {
    let db = fixture!(
        "count",
        "create table t (id integer primary key, v text);
         insert into t (v) with recursive c(x) as (
           select 1 union all select x + 1 from c where x < 30000
         ) select 'row ' || x from c;
         create table empty (a int);"
    );
    assert_eq!(fenec_import::sqlite::count_rows(&db, "t").unwrap(), 30_000);
    assert_eq!(fenec_import::sqlite::count_rows(&db, "empty").unwrap(), 0);
    assert!(fenec_import::sqlite::count_rows(&db, "nosuch").is_err());
}
