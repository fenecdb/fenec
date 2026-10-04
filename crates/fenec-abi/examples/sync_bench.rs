//! What the sync core costs a change it applies (`make sync-bench`): the
//! bytes of a change event fed to `Sync::bytes` -- read off the stream,
//! parsed, written to the replica with the shape's cursor in one block --
//! against the same rows put straight into the engine, one statement a
//! change, which is the least a replica could do. A row of text, a row
//! with a 128-dim vector under HNSW, changes of one row and of 100, and a
//! seed of 10 000 rows over the 10 000 there. Over a file, as an app keeps
//! its replica, the writes buffered as the stream's are (no fsync).

use fenec_abi::sync::Sync;
use fenec_core::prelude::*;
use std::time::Instant;

const ONE: u64 = 5_000;
const BATCHES: u64 = 50;

fn main() {
    let dir = std::env::temp_dir().join(format!("fenec-sync-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    type Case = (&'static str, &'static str, fn(u64) -> String);
    let cases: [Case; 2] = [
        (
            "text row",
            "create collection t (key text @hash, title text, n int)",
            text_row,
        ),
        (
            "128-dim row under HNSW",
            "create collection t (key text @hash, title text, e vector<128> @hnsw(cosine))",
            vector_row,
        ),
    ];
    for (name, schema, row) in cases {
        let path = dir.join("replica.fenec");
        let direct = measure(&path, schema, row, false);
        let synced = measure(&path, schema, row, true);
        println!(
            "{name}: a change of one row {:.1} us (the engine alone {:.1}), of 100 rows {:.1} us a row ({:.1}), \
             a seed of 10 000 rows {:.1} ms ({:.1})",
            synced.0 * 1e6,
            direct.0 * 1e6,
            synced.1 * 1e6,
            direct.1 * 1e6,
            synced.2 * 1e3,
            direct.2 * 1e3,
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Seconds a change of one row, a row in changes of 100, and a seed.
fn measure(
    path: &std::path::Path,
    schema: &str,
    row: fn(u64) -> String,
    sync: bool,
) -> (f64, f64, f64) {
    let _ = std::fs::remove_file(path);
    let mut db = fenec_core::fs::open(path).unwrap();
    db.execute(&fenec_ql::parse_one(schema).unwrap()).unwrap();
    let mut s = Sync::start(
        &mut db,
        r#"{"url":"http://s","seed":"00","shapes":[{"collection":"t","key":"key"}]}"#,
    )
    .unwrap();
    s.actions();
    s.opened(&mut db, 1, 200, "");
    s.bytes(
        &mut db,
        1,
        b"event: seed\ndata: {\"seq\":1,\"rows\":[]}\n\n",
    );
    s.actions();

    let mut apply = |db: &mut Database, seq: u64, rows: &str, seed: bool| {
        if sync {
            let e = match seed {
                true => format!("event: seed\ndata: {{\"seq\":{seq},\"rows\":[{rows}]}}\n\n"),
                false => format!(
                    "event: change\ndata: {{\"seq\":{seq},\"puts\":[{rows}],\"dels\":[],\"schema\":false}}\n\n"
                ),
            };
            s.bytes(db, 1, e.as_bytes());
            s.actions();
        } else {
            let docs = fenec_core::json::parse_documents_json(&format!("[{rows}]"), &[]).unwrap();
            let put = Statement::Put {
                collection: "t".into(),
                docs: docs
                    .into_iter()
                    .map(|d| d.into_iter().map(|(k, v)| (k, Expr::Lit(v))).collect())
                    .collect(),
                insert: false,
                if_absent: false,
            };
            db.execute_with(&put, &[]).unwrap();
        }
    };

    let events: Vec<String> = (0..ONE).map(|i| row(i + 1)).collect();
    let t = Instant::now();
    for (i, e) in events.iter().enumerate() {
        apply(&mut db, i as u64 + 2, e, false);
    }
    let one = t.elapsed().as_secs_f64() / ONE as f64;

    let events: Vec<String> = (0..BATCHES)
        .map(|b| {
            let rows: Vec<String> = (0..100).map(|i| row(ONE + 1 + b * 100 + i)).collect();
            rows.join(",")
        })
        .collect();
    let t = Instant::now();
    for (b, e) in events.iter().enumerate() {
        apply(&mut db, ONE + 2 + b as u64, e, false);
    }
    let hundred = t.elapsed().as_secs_f64() / (BATCHES * 100) as f64;

    let rows: Vec<String> = (1..=10_000).map(row).collect();
    let rows = rows.join(",");
    let t = Instant::now();
    apply(&mut db, ONE + BATCHES + 10, &rows, true);
    let seeded = t.elapsed().as_secs_f64();
    drop(s);
    drop(db);
    let _ = std::fs::remove_file(path);
    (one, hundred, seeded)
}

fn text_row(id: u64) -> String {
    format!(
        r#"{{"id":{id},"key":"k{id}","title":"a title of some forty characters, {id}","n":{id}}}"#
    )
}

fn vector_row(id: u64) -> String {
    let mut x = id.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let e: Vec<String> = (0..128)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            format!("{:.6}", (x % 2000) as f32 / 1000.0 - 1.0)
        })
        .collect();
    format!(
        r#"{{"id":{id},"key":"k{id}","title":"t{id}","e":[{}]}}"#,
        e.join(",")
    )
}
