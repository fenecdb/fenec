//! tokio-postgres over fenec-server's pg wire: typed parameters, typed rows in
//! the binary format, a COPY through Execute, and pgvector-rust's types.

use futures_util::{SinkExt, TryStreamExt};
use pgvector::{HalfVector, SparseVector, Vector};
use tokio_postgres::binary_copy::{BinaryCopyInWriter, BinaryCopyOutStream};
use tokio_postgres::types::{Kind, Type};
use tokio_postgres::{Client, NoTls};

/// A connection, and a collection of the test's own: `test` in its name,
/// since two tests start within the clock's microsecond.
async fn connect(test: &str) -> Option<(Client, String)> {
    let dsn = std::env::var("FENEC_PG").ok()?;
    let (c, conn) = tokio_postgres::connect(&dsn, NoTls).await.unwrap();
    tokio::spawn(conn);
    let name = format!(
        "rs_{test}_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    c.simple_query(&format!(
        "create collection {name} (name text, n int, score float, ok bool, e vector<2>)"
    ))
    .await
    .unwrap();
    Some((c, name))
}

#[tokio::test(flavor = "current_thread")]
async fn typed_parameters_and_rows() {
    let Some((c, t)) = connect("typed").await else {
        eprintln!("FENEC_PG is not set: integrations/drivers/run-tests.sh sets it");
        return;
    };
    let n = c
        .execute(
            &format!("put {t} {{name: $1, n: $2, score: $3, ok: $4, e: $5}}"),
            &[&"a", &7i64, &0.25f64, &true, &Vector::from(vec![1.0, 0.5])],
        )
        .await
        .unwrap();
    assert_eq!(n, 1);
    let row = c
        .query_one(
            &format!("get {t} select name, n, score, ok, e where n = $1 and name = $2"),
            &[&7i64, &"a"],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "a");
    assert_eq!(row.get::<_, i64>(1), 7);
    assert_eq!(row.get::<_, f64>(2), 0.25);
    assert!(row.get::<_, bool>(3));
    assert_eq!(row.get::<_, Vector>(4).to_vec(), [1.0, 0.5]);
    let count: i64 = c
        .query_one(&format!("get {t} count"), &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 1);
}

#[tokio::test(flavor = "current_thread")]
async fn copy_in_goes_through_execute() {
    let Some((c, t)) = connect("copy").await else {
        return;
    };
    let sink = c
        .copy_in(&format!("COPY {t} (name, n) FROM STDIN"))
        .await
        .unwrap();
    futures_util::pin_mut!(sink);
    sink.send(bytes::Bytes::from_static(b"r1\t100\nr2\t101\n"))
        .await
        .unwrap();
    assert_eq!(sink.finish().await.unwrap(), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn a_binary_copy_writes_typed_rows() {
    let Some((c, t)) = connect("binary").await else {
        return;
    };
    let sink = c
        .copy_in(&format!("COPY {t} (name, n, score) FROM STDIN BINARY"))
        .await
        .unwrap();
    let writer = BinaryCopyInWriter::new(sink, &[Type::TEXT, Type::INT8, Type::FLOAT8]);
    futures_util::pin_mut!(writer);
    for i in 0..100i64 {
        writer
            .as_mut()
            .write(&[&format!("r{i}"), &i, &(i as f64 / 4.0)])
            .await
            .unwrap();
    }
    assert_eq!(writer.finish().await.unwrap(), 100);
    let row = c
        .query_one(
            &format!("get {t} select name, score where n = $1"),
            &[&7i64],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "r7");
    assert_eq!(row.get::<_, f64>(1), 1.75);
}

/// pgvector-rust's `Vector`, `HalfVector` and `SparseVector`, which name
/// the types they go as, in and out in pgvector's binary formats -- and its
/// bulk load, the type found by name in the catalog.
#[tokio::test(flavor = "current_thread")]
async fn pgvectors_types_go_both_ways() {
    let Some((c, t)) = connect("pgvector").await else {
        return;
    };
    c.simple_query(&format!(
        "create collection {t}_all (name text, e vector<3> @hnsw(cosine), h vector<3, f16>, s sparse<5> @inverted)"
    ))
    .await
    .unwrap();
    let t = format!("{t}_all");
    let e = Vector::from(vec![1.0, 2.0, 3.0]);
    let h = HalfVector::from_f32_slice(&[1.5, 2.0, 3.0]);
    let s = SparseVector::from_dense(&[1.0, 0.0, 0.0, 0.5, 0.0]);
    c.execute(
        &format!("put {t} {{name: $1, e: $2, h: $3, s: $4}}"),
        &[&"a", &e, &h, &s],
    )
    .await
    .unwrap();
    let row = c
        .query_one(&format!("get {t} select e, h, s where name = $1"), &[&"a"])
        .await
        .unwrap();
    assert_eq!(row.get::<_, Vector>(0), e);
    assert_eq!(row.get::<_, HalfVector>(1), h);
    assert_eq!(row.get::<_, SparseVector>(2), s);
    let hit = c
        .query_one(&format!("get {t} select name near e $1 limit 1"), &[&e])
        .await
        .unwrap();
    assert_eq!(hit.get::<_, String>(0), "a");
    let query = SparseVector::from_dense(&[1.0, 0.0, 0.0, 0.0, 0.0]);
    let hit = c
        .query_one(&format!("get {t} select name near s $1 limit 1"), &[&query])
        .await
        .unwrap();
    assert_eq!(hit.get::<_, String>(0), "a");

    // pgvector-rust's bulk load: the type by name, then a binary COPY.
    let found = c
        .query_one(
            "SELECT pg_type.oid, nspname AS schema FROM pg_type \
             INNER JOIN pg_namespace ON pg_namespace.oid = pg_type.typnamespace \
             WHERE typname = $1",
            &[&"vector"],
        )
        .await
        .unwrap();
    let vector = Type::new(
        "vector".into(),
        found.get("oid"),
        Kind::Simple,
        found.get("schema"),
    );
    let sink = c
        .copy_in(&format!(
            "COPY {t} (name, e) FROM STDIN WITH (FORMAT BINARY)"
        ))
        .await
        .unwrap();
    let writer = BinaryCopyInWriter::new(sink, &[Type::TEXT, vector]);
    futures_util::pin_mut!(writer);
    for i in 0..100 {
        let v = Vector::from(vec![1.0, i as f32, 2.0]);
        writer
            .as_mut()
            .write(&[&format!("c{i}"), &v])
            .await
            .unwrap();
    }
    assert_eq!(writer.finish().await.unwrap(), 100);
    let row = c
        .query_one(&format!("get {t} select e where name = $1"), &[&"c7"])
        .await
        .unwrap();
    assert_eq!(row.get::<_, Vector>(0).to_vec(), [1.0, 7.0, 2.0]);
}

#[tokio::test(flavor = "current_thread")]
async fn lists_are_arrays() {
    let Some((c, name)) = connect("lists").await else {
        return;
    };
    let t = format!("{name}_arrays");
    c.simple_query(&format!(
        "create collection {t} (name text, tags [text], ns [int], fs [float])"
    ))
    .await
    .unwrap();
    let tags = vec!["plain", "a \"q\"", "b\\s", "x,y", "", "NULL"];
    c.execute(
        &format!("put {t} {{name: $1, tags: $2, ns: $3, fs: $4}}"),
        &[&"a", &tags, &vec![1i64, -2, 3], &vec![0.5f64, 1.25]],
    )
    .await
    .unwrap();
    let row = c
        .query_one(
            &format!("get {t} select tags, ns, fs where name = $1"),
            &[&"a"],
        )
        .await
        .unwrap();
    assert_eq!(row.columns()[0].type_(), &Type::TEXT_ARRAY);
    assert_eq!(row.get::<_, Vec<String>>(0), tags);
    assert_eq!(row.get::<_, Vec<i64>>(1), vec![1, -2, 3]);
    assert_eq!(row.get::<_, Vec<f64>>(2), vec![0.5, 1.25]);
}

#[tokio::test(flavor = "current_thread")]
async fn a_binary_copy_reads_typed_rows_out() {
    let Some((c, t)) = connect("copyout").await else {
        return;
    };
    for i in 0..1_500i64 {
        c.execute(
            &format!("put {t} {{name: $1, n: $2, score: $3, e: $4}}"),
            &[
                &format!("r{i}"),
                &i,
                &(i as f64 / 4.0),
                &Vector::from(vec![i as f32, 1.0]),
            ],
        )
        .await
        .unwrap();
    }
    // pgvector's type, as the server names it, looked up before the COPY:
    // a query behind it on the same connection waits for it to be read.
    let vector = c
        .query_one(&format!("get {t} select e limit 1"), &[])
        .await
        .unwrap()
        .columns()[0]
        .type_()
        .clone();
    let stream = c
        .copy_out(&format!(
            "COPY {t} (name, n, score, e) TO STDOUT (FORMAT binary)"
        ))
        .await
        .unwrap();
    let rows = BinaryCopyOutStream::new(stream, &[Type::TEXT, Type::INT8, Type::FLOAT8, vector]);
    futures_util::pin_mut!(rows);
    let mut n = 0i64;
    while let Some(row) = rows.try_next().await.unwrap() {
        assert_eq!(row.get::<String>(0), format!("r{n}"));
        assert_eq!(row.get::<i64>(1), n);
        assert_eq!(row.get::<f64>(2), n as f64 / 4.0);
        assert_eq!(row.get::<Vector>(3).to_vec(), vec![n as f32, 1.0]);
        n += 1;
    }
    assert_eq!(n, 1_500);
}
