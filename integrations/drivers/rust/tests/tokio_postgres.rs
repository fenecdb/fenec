//! tokio-postgres over fenec-pg's pg wire: typed parameters, typed rows in
//! the binary format, and a COPY through Execute.

use futures_util::SinkExt;
use tokio_postgres::binary_copy::BinaryCopyInWriter;
use tokio_postgres::types::Type;
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
            &[&"a", &7i64, &0.25f64, &true, &"[1,0.5]"],
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
    assert_eq!(row.get::<_, String>(4), "[1,0.5]");
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
        .query_one(&format!("get {t} select name, score where n = $1"), &[&7i64])
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "r7");
    assert_eq!(row.get::<_, f64>(1), 1.75);
}
