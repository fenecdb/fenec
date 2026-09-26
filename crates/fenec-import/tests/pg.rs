//! Verifies the PostgreSQL arm against a live server.
//!
//! Marked `#[ignore]`: it needs a running server. The ready-made server in
//! the Makefile is enough --
//!
//! ```text
//! make pgvector-up
//! cargo test -p fenec-import --test all pg:: -- --ignored
//! make pgvector-down
//! ```
//!
//! `FENEC_TEST_PG_URL` can point at another server. Since PostgreSQL 17 speaks
//! scram-sha-256 by default, this test also exercises the client's SCRAM
//! half against *real* PostgreSQL.

use fenec_core::prelude::*;
use fenec_core::query::Statement as Stmt;
use fenec_core::value::VecPrec;
use fenec_import::pg::{Query, Reader, Url};
use fenec_import::{load, map, Options, Source};
use fenec_wire::client::Client;

/// The server the Makefile's `pgvector-up` target starts.
const DEFAULT_URL: &str = "postgres://postgres:fenec@127.0.0.1:55432/fenecbench";

fn url() -> Url {
    let s = std::env::var("FENEC_TEST_PG_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    Url::parse(&s).expect("invalid FENEC_TEST_PG_URL")
}

/// The extension is created once: `create extension if not exists` is not
/// atomic against concurrent calls and returns 23505 in parallel tests.
static EXTENSION: std::sync::Once = std::sync::Once::new();

/// Sets up the fixture. The table name is unique per test so the tests can
/// run in parallel.
fn setup(table: &str, ddl: &str) {
    EXTENSION.call_once(|| {
        let mut c = Client::connect(&url()).expect("could not connect (make pgvector-up?)");
        let _ = c.query("create extension if not exists vector");
    });
    let mut c = Client::connect(&url()).expect("could not connect (make pgvector-up?)");
    c.query(&format!("drop table if exists {table}")).unwrap();
    c.query(ddl).unwrap();
}

fn read_all(table: &str) -> (Vec<fenec_import::Column>, Vec<Vec<Value>>) {
    let mut r = Reader::open(&url(), &Query::table(table)).unwrap();
    let cols = r.columns().unwrap();
    let mut rows = Vec::new();
    while let Some(row) = r.next_row().unwrap() {
        rows.push(row);
    }
    (cols, rows)
}

#[test]
#[ignore]
fn every_mapped_type_round_trips() {
    let t = "fenec_test_types";
    setup(
        t,
        &format!(
            "create table {t} (
               id       bigint,
               name     text,
               short    varchar(16),
               number   int,
               big      bigint,
               ratio    double precision,
               active   boolean,
               tags     text[],
               numbers  int[],
               data     bytea,
               moment   timestamptz,
               day      date,
               ident    uuid
             )"
        ),
    );
    let mut c = Client::connect(&url()).unwrap();
    c.query(&format!(
        "insert into {t} values
           (1, 'one', 'short', 42, 9223372036854775807, 2.5, true,
            '{{\"a,b\",\"c\\\"d\"}}', '{{1,2,3}}', '\\x48656c6c6f',
            '2026-09-19 12:34:56.789+00', '2026-09-19',
            '00000000-0000-0000-0000-000000000001'),
           (2, NULL, NULL, NULL, NULL, NULL, false, '{{}}', NULL, '\\x', NULL, NULL, NULL)"
    ))
    .unwrap();

    let (cols, rows) = read_all(t);
    let ty = |n: &str| {
        cols.iter()
            .find(|c| c.name == n)
            .unwrap_or_else(|| panic!("there is no `{n}` column"))
            .ty
            .clone()
    };
    assert_eq!(ty("id"), Some(DataType::Int));
    assert_eq!(ty("name"), Some(DataType::Text));
    assert_eq!(ty("short"), Some(DataType::Text));
    assert_eq!(ty("ratio"), Some(DataType::Float));
    assert_eq!(ty("active"), Some(DataType::Bool));
    assert_eq!(ty("tags"), Some(DataType::List(Box::new(DataType::Text))));
    assert_eq!(ty("numbers"), Some(DataType::List(Box::new(DataType::Int))));
    assert_eq!(ty("data"), Some(DataType::Bytes));
    assert_eq!(ty("moment"), Some(DataType::Timestamp));
    assert_eq!(ty("day"), Some(DataType::Timestamp));
    assert_eq!(ty("ident"), Some(DataType::Text));

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::Int(1));
    assert_eq!(rows[0][4], Value::Int(i64::MAX));
    assert_eq!(rows[0][6], Value::Bool(true));
    // Quoted array items: comma and quote escapes must be decoded.
    assert_eq!(
        rows[0][7],
        Value::List(vec![Value::Text("a,b".into()), Value::Text("c\"d".into())])
    );
    assert_eq!(rows[0][9], Value::Bytes(b"Hello".to_vec()));
    // The timestamp passes through as text; `coerce` parses it.
    assert_eq!(
        rows[0][10],
        Value::Text("2026-09-19 12:34:56.789+00".into())
    );

    // The second row is full of NULLs.
    assert_eq!(rows[1][1], Value::Null);
    assert_eq!(rows[1][6], Value::Bool(false));
    assert_eq!(
        rows[1][7],
        Value::List(vec![]),
        "an empty array is not NULL"
    );
    assert_eq!(rows[1][8], Value::Null);
    assert_eq!(
        rows[1][9],
        Value::Bytes(vec![]),
        "an empty bytea is not NULL"
    );
}

/// pgvector's OID belongs to an extension type; the dimension comes from `typmod`.
#[test]
#[ignore]
fn pgvector_columns_become_vectors() {
    let t = "fenec_test_vector";
    setup(
        t,
        &format!("create table {t} (id bigint, title text, embed vector(3))"),
    );
    let mut c = Client::connect(&url()).unwrap();
    c.query(&format!(
        "insert into {t} values (1,'one','[1,0,0]'), (2,'two','[0,1,0]'), (3,'three','[0,0,1]')"
    ))
    .unwrap();

    let (cols, _) = read_all(t);
    assert_eq!(
        cols[2].ty,
        Some(DataType::Vector(3, VecPrec::F32)),
        "the dimension must be read from typmod"
    );

    let mut src = Reader::open(&url(), &Query::table(t)).unwrap();
    let mut db = Database::new();
    let mut o = Options::new("m");
    o.indexes.push((
        "embed".into(),
        IndexKind::Vector(VectorIndexSpec::default()),
    ));
    assert_eq!(load::run(&mut src, &mut db, &o).unwrap().rows, 3);
    assert_eq!(db.stats()[0].vector_indexes[0].dim, 3);

    let Response::Rows(rs) = db
        .execute(&Statement::Select(Select {
            collection: "m".into(),
            project: Some(vec!["title".into()]),
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
    assert_eq!(rs.rows[0].values[0], Value::Text("two".into()));
}

/// `numeric` must not silently fall back to float; `text` keeps the exact
/// value as PostgreSQL has it.
#[test]
#[ignore]
fn numeric_demands_cast_and_text_is_exact() {
    let t = "fenec_test_money";
    setup(
        t,
        &format!("create table {t} (id bigint, price numeric(10,2))"),
    );
    let mut c = Client::connect(&url()).unwrap();
    c.query(&format!("insert into {t} values (1, 19.99), (2, 29.50)"))
        .unwrap();

    let (cols, _) = read_all(t);
    let e = map::plan(&cols, &Options::new("m"))
        .unwrap_err()
        .to_string();
    assert!(e.contains("--cast price="), "{e}");

    let mut src = Reader::open(&url(), &Query::table(t)).unwrap();
    let mut db = Database::new();
    let mut o = Options::new("m");
    o.casts.push(("price".into(), DataType::Text));
    load::run(&mut src, &mut db, &o).unwrap();

    let Response::Rows(rs) = db
        .execute(&Statement::Select(Select {
            collection: "m".into(),
            project: Some(vec!["price".into()]),
            order: vec![Sort::new("price", true)],
            ..Default::default()
        }))
        .unwrap()
    else {
        panic!()
    };
    let got: Vec<&Value> = rs.rows.iter().map(|r| &r.values[0]).collect();
    assert_eq!(
        got,
        vec![&Value::Text("19.99".into()), &Value::Text("29.50".into())],
        "the scale must be preserved: 29.50 must not become `29.5`"
    );
}

/// `--where` and `--limit` run on the source: unnecessary rows never stream.
#[test]
#[ignore]
fn filter_and_limit_run_on_the_server() {
    let t = "fenec_test_filter";
    setup(t, &format!("create table {t} (id bigint, k text)"));
    let mut c = Client::connect(&url()).unwrap();
    c.query(&format!(
        "insert into {t} select i, case when i % 2 = 0 then 'even' else 'odd' end
         from generate_series(1, 1000) i"
    ))
    .unwrap();

    let mut r = Reader::open(
        &url(),
        &Query {
            table: t.into(),
            filter: Some("k = 'even'".into()),
            limit: Some(10),
        },
    )
    .unwrap();
    let mut n = 0;
    while let Some(row) = r.next_row().unwrap() {
        assert_eq!(row[1], Value::Text("even".into()));
        n += 1;
    }
    assert_eq!(n, 10);
}

/// The COPY stream does not fit in one frame; in a large table rows cross
/// frame boundaries.
#[test]
#[ignore]
fn large_table_streams_correctly() {
    let t = "fenec_test_large";
    setup(t, &format!("create table {t} (id bigint, title text)"));
    let mut c = Client::connect(&url()).unwrap();
    c.query(&format!(
        "insert into {t} select i, 'document ' || i from generate_series(1, 50000) i"
    ))
    .unwrap();

    let mut src = Reader::open(&url(), &Query::table(t)).unwrap();
    let mut db = Database::new();
    let s = load::run(&mut src, &mut db, &Options::new("m")).unwrap();
    assert_eq!(s.rows, 50_000);
    assert_eq!(db.stats()[0].documents, 50_000);

    // The source ids must be preserved.
    let Response::Rows(rs) = db
        .execute(&Statement::Select(Select {
            collection: "m".into(),
            project: Some(vec!["title".into()]),
            filter: Some(Expr::Cmp(
                CmpOp::Eq,
                Box::new(Expr::Field("title".into())),
                Box::new(Expr::Lit(Value::Text("document 49999".into()))),
            )),
            ..Default::default()
        }))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(rs.rows.len(), 1);
}

#[test]
#[ignore]
fn missing_table_is_reported() {
    let e = Reader::open(&url(), &Query::table("fenec_test_no_such"))
        .err()
        .expect("a missing table must error")
        .to_string();
    assert!(e.contains("42P01") || e.contains("does not exist"), "{e}");
}

/// Two filters together: `--source-where` cuts on the server and `--where`
/// filters locally. They do not speak the same language, and they should not
/// -- one is SQL, the other FenecQL.
#[test]
#[ignore]
fn source_filter_and_local_filter_compose() {
    let t = "fenec_test_two_filters";
    setup(
        t,
        &format!("create table {t} (id bigint, category text, score int)"),
    );
    let mut c = Client::connect(&url()).unwrap();
    c.query(&format!(
        "insert into {t} select i,
           case when i % 2 = 0 then 'even' else 'odd' end, i
         from generate_series(1, 100) i"
    ))
    .unwrap();

    // On the server: only 'even' (50 rows stream).
    let mut src = Reader::open(
        &url(),
        &Query {
            table: t.into(),
            filter: Some("category = 'even'".into()),
            limit: None,
        },
    )
    .unwrap();

    // Locally: score > 90 (of the even ones, 92..100 -> 5 rows).
    let Stmt::Select(sel) = fenec_ql::parse_one("get t where score > 90").unwrap() else {
        panic!()
    };
    let mut db = Database::new();
    let mut o = Options::new("m");
    o.filter = sel.filter;
    let s = load::run(&mut src, &mut db, &o).unwrap();
    assert_eq!(s.rows, 5, "92, 94, 96, 98, 100");
    assert_eq!(
        s.skipped, 45,
        "45 of the 50 rows from the server were dropped"
    );
}

/// The count must see the same narrowing as `--source-where` and `--limit`;
/// otherwise `--dry-run` reports more than will actually be read.
#[test]
#[ignore]
fn count_respects_filter_and_limit() {
    let t = "fenec_test_count";
    setup(t, &format!("create table {t} (id bigint, k text)"));
    let mut c = Client::connect(&url()).unwrap();
    c.query(&format!(
        "insert into {t} select i, case when i % 4 = 0 then 'four' else 'other' end
         from generate_series(1, 1000) i"
    ))
    .unwrap();

    let all = Query::table(t);
    assert_eq!(fenec_import::pg::count_rows(&url(), &all).unwrap(), 1000);

    let filtered = Query {
        table: t.into(),
        filter: Some("k = 'four'".into()),
        limit: None,
    };
    assert_eq!(
        fenec_import::pg::count_rows(&url(), &filtered).unwrap(),
        250
    );

    let capped = Query {
        table: t.into(),
        filter: Some("k = 'four'".into()),
        limit: Some(10),
    };
    assert_eq!(fenec_import::pg::count_rows(&url(), &capped).unwrap(), 10);
}
