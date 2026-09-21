//! fenecdb compared with SQLite.
//!
//! Fairness rules
//! - Same data, same process, same allocator.
//! - Vector distance is computed with the `fenec_core::vector` kernel on both
//!   sides; what is measured is the engine's *data fetching* cost, not the
//!   arithmetic.
//! - SQLite does the bulk write in a single transaction and builds the index
//!   afterwards (the recommended usage). WAL + `synchronous=NORMAL`: fenecdb
//!   does not fsync on every write either, so the durability models match.
//! - The SQLite core has no ANN index; vector search is a full scan. So
//!   **exact against exact** is compared first (equal semantics), and
//!   fenecdb's ANN is shown as a separate row.
//! - PostgreSQL computes its distances inside pgvector, not with our kernel.
//!   What it shares with fenecdb is the HNSW build (`m`, `ef_construction`,
//!   one process per core), the search beam (`ef_search`) and the data.
//!
//! The PostgreSQL arm needs a running pgvector:
//! ```text
//! docker run -d --name fenecbench-pg -e POSTGRES_PASSWORD=fenec -e POSTGRES_DB=fenecbench \
//!   -p 55432:5432 --shm-size=1g pgvector/pgvector:pg17 \
//!   -c shared_buffers=1GB -c maintenance_work_mem=1GB -c max_parallel_workers_per_gather=0
//! ```
//! If it is unreachable, that arm is skipped. Because PostgreSQL is
//! client-server, every query pays a TCP round trip; the empty round trip is
//! therefore measured and reported separately.
//!
//! `cargo run --release -p fenec-bench -- [N] [DIM]`

use fenec_core::prelude::*;
use fenec_core::vector::{dot, normalized};
use postgres::{Client, NoTls};
use rusqlite::{params, Connection};
use std::time::Instant;

struct Rng(u64);
impl Rng {
    fn f32(&mut self) -> f32 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        ((x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as f32) / ((1u32 << 24) as f32)
    }
    fn gauss(&mut self) -> f32 {
        (0..6).map(|_| self.f32()).sum::<f32>() - 3.0
    }
}

struct Row {
    category: &'static str,
    score: i64,
    embed: Vec<f32>,
}

const CATEGORIES: [&str; 4] = ["a", "b", "c", "d"];

fn generate(n: usize, dim: usize) -> (Vec<Row>, Vec<Vec<f32>>) {
    let mut rng = Rng(0xC0FFEE);
    let centers: Vec<Vec<f32>> = (0..64)
        .map(|_| (0..dim).map(|_| rng.gauss()).collect())
        .collect();
    let mut rows = Vec::with_capacity(n);
    let mut queries = Vec::new();
    for i in 0..n {
        let c = &centers[i % centers.len()];
        let embed: Vec<f32> = c.iter().map(|x| x + rng.gauss() * 0.35).collect();
        if i % (n / 50).max(1) == 0 && queries.len() < 50 {
            queries.push(embed.clone());
        }
        rows.push(Row {
            category: CATEGORIES[i % 4],
            score: (i % 1000) as i64,
            embed,
        });
    }
    (rows, queries)
}

fn ms(d: std::time::Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn p50(v: &mut Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

/// Finds the k nearest neighbours by full scan (code shared by both engines).
fn topk_exact(qn: &[f32], vectors: impl Iterator<Item = (u64, Vec<f32>)>, k: usize) -> Vec<u64> {
    let mut best: Vec<(f32, u64)> = Vec::with_capacity(k + 1);
    for (id, v) in vectors {
        let d = 1.0 - dot(qn, &normalized(&v));
        if best.len() < k {
            best.push((d, id));
            best.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        } else if d < best[k - 1].0 {
            best[k - 1] = (d, id);
            best.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        }
    }
    best.into_iter().map(|(_, id)| id).collect()
}

fn file_size(p: &str) -> u64 {
    let mut total = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    for suffix in ["-wal", "-shm"] {
        total += std::fs::metadata(format!("{p}{suffix}"))
            .map(|m| m.len())
            .unwrap_or(0);
    }
    total
}

// ---------------------------------------------------------------- fenecdb

struct Result_ {
    /// Writing the rows (without building the vector index).
    ingest_ms: f64,
    /// Building the indexes.
    index_ms: f64,
    insert_ms: f64,
    bytes: u64,
    scalar_p50: f64,
    exact_p50: f64,
    ann_p50: Option<f64>,
    reopen_ms: Option<f64>,
    recall: Option<f64>,
    /// Round-trip time of an empty query on client-server engines.
    rtt: Option<f64>,
    /// Where the on-disk bytes go, for an engine that can say.
    breakdown: Option<String>,
}

fn run_fenecdb(path: &str, rows: &[Row], queries: &[Vec<f32>], dim: usize) -> Result_ {
    let _ = std::fs::remove_file(path);
    let mut db = fenec_core::fs::open(path).unwrap();
    // Schema without indexes: the same workflow as SQLite and PostgreSQL --
    // bulk load first, then index build.
    db.execute(&Statement::CreateCollection {
        schema: Schema::new(
            "docs",
            vec![
                Field::new("category", DataType::Text),
                Field::new("score", DataType::Int),
                Field::new("embed", DataType::Vector(dim, VecPrec::F32)),
            ],
        )
        .unwrap(),
        if_not_exists: false,
    })
    .unwrap();

    let t = Instant::now();
    for chunk in rows.chunks(2_000) {
        let docs: Vec<Vec<(String, Expr)>> = chunk
            .iter()
            .map(|r| {
                vec![
                    ("category".into(), Expr::Lit(Value::Text(r.category.into()))),
                    ("score".into(), Expr::Lit(Value::Int(r.score))),
                    ("embed".into(), Expr::Lit(Value::Vector(r.embed.clone()))),
                ]
            })
            .collect();
        db.execute(&Statement::Put {
            collection: "docs".into(),
            docs,
        })
        .unwrap();
    }
    let ingest_ms = ms(t.elapsed());

    let ti = Instant::now();
    db.execute(&Statement::CreateIndex {
        collection: "docs".into(),
        field: "category".into(),
        kind: IndexKind::Hash,
        if_not_exists: false,
    })
    .unwrap();
    db.execute(&Statement::CreateIndex {
        collection: "docs".into(),
        field: "embed".into(),
        kind: IndexKind::Vector(VectorIndexSpec::default()),
        if_not_exists: false,
    })
    .unwrap();
    let index_ms = ms(ti.elapsed());
    db.checkpoint().unwrap();
    let insert_ms = ms(t.elapsed());
    let bytes = file_size(path);

    // scalar: category equality (hash index) + range
    let scalar = Select {
        collection: "docs".into(),
        project: Some(vec!["id".into()]),
        filter: Some(Expr::And(
            Box::new(Expr::Cmp(
                CmpOp::Eq,
                Box::new(Expr::Field("category".into())),
                Box::new(Expr::Lit(Value::Text("a".into()))),
            )),
            Box::new(Expr::Cmp(
                CmpOp::Gt,
                Box::new(Expr::Field("score".into())),
                Box::new(Expr::Lit(Value::Int(500))),
            )),
        )),
        ..Default::default()
    };
    let mut lat = Vec::new();
    for _ in 0..20 {
        let t = Instant::now();
        let r = db.execute(&Statement::Select(scalar.clone())).unwrap();
        lat.push(ms(t.elapsed()));
        assert!(r.rows().unwrap().rows.len() > 0);
    }
    let scalar_p50 = p50(&mut lat);

    let mk = |q: &Vec<f32>, exact: bool| Select {
        collection: "docs".into(),
        project: Some(vec!["id".into()]),
        near: Some(Near {
            field: "embed".into(),
            vector: Expr::Lit(Value::Vector(q.clone())),
            ef: None,
            exact,
        }),
        limit: Some(10),
        ..Default::default()
    };

    let mut lat = Vec::new();
    for q in queries {
        let t = Instant::now();
        db.execute(&Statement::Select(mk(q, true))).unwrap();
        lat.push(ms(t.elapsed()));
    }
    let exact_p50 = p50(&mut lat);

    let mut lat = Vec::new();
    let mut hits = 0usize;
    let mut total = 0usize;
    for q in queries {
        let t = Instant::now();
        let a = db.execute(&Statement::Select(mk(q, false))).unwrap();
        lat.push(ms(t.elapsed()));
        let e = db.execute(&Statement::Select(mk(q, true))).unwrap();
        let ids: Vec<u64> = e.rows().unwrap().rows.iter().map(|r| r.id).collect();
        hits += a
            .rows()
            .unwrap()
            .rows
            .iter()
            .filter(|r| ids.contains(&r.id))
            .count();
        total += ids.len();
    }
    let ann_p50 = p50(&mut lat);
    drop(db);

    let t = Instant::now();
    let mut db2 = fenec_core::fs::open(path).unwrap();
    db2.execute(&Statement::Select(mk(&queries[0], false)))
        .unwrap();
    let reopen_ms = ms(t.elapsed());

    Result_ {
        ingest_ms,
        index_ms,
        insert_ms,
        bytes,
        scalar_p50,
        exact_p50,
        ann_p50: Some(ann_p50),
        reopen_ms: Some(reopen_ms),
        recall: Some(hits as f64 / total as f64 * 100.0),
        rtt: None,
        breakdown: None,
    }
}

// --------------------------------------------------------------- sqlite

fn blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}
fn unblob(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
        .collect()
}

fn run_sqlite(path: &str, rows: &[Row], queries: &[Vec<f32>]) -> Result_ {
    for s in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{path}{s}"));
    }
    let conn = Connection::open(path).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.pragma_update(None, "synchronous", "NORMAL").unwrap();
    conn.execute_batch(
        "CREATE TABLE docs (
           id       INTEGER PRIMARY KEY,
           category TEXT    NOT NULL,
           score    INTEGER NOT NULL,
           embed    BLOB    NOT NULL
         );",
    )
    .unwrap();

    let t = Instant::now();
    {
        let tx = conn.unchecked_transaction().unwrap();
        {
            let mut stmt = tx
                .prepare("INSERT INTO docs (category, score, embed) VALUES (?1, ?2, ?3)")
                .unwrap();
            for r in rows {
                stmt.execute(params![r.category, r.score, blob(&r.embed)])
                    .unwrap();
            }
        }
        tx.commit().unwrap();
    }
    let ingest_ms = ms(t.elapsed());
    // The index is built after the bulk write (the recommended order).
    let ti = Instant::now();
    conn.execute_batch("CREATE INDEX idx_category ON docs(category);")
        .unwrap();
    let index_ms = ms(ti.elapsed());
    let insert_ms = ms(t.elapsed());
    conn.pragma_update(None, "wal_checkpoint", "TRUNCATE").ok();
    let bytes = file_size(path);

    let mut lat = Vec::new();
    for _ in 0..20 {
        let t = Instant::now();
        let mut stmt = conn
            .prepare_cached("SELECT id FROM docs WHERE category = ?1 AND score > ?2")
            .unwrap();
        let n: usize = stmt
            .query_map(params!["a", 500i64], |r| r.get::<_, i64>(0))
            .unwrap()
            .count();
        lat.push(ms(t.elapsed()));
        assert!(n > 0);
    }
    let scalar_p50 = p50(&mut lat);

    let mut lat = Vec::new();
    for q in queries {
        let qn = normalized(q);
        let t = Instant::now();
        let mut stmt = conn.prepare_cached("SELECT id, embed FROM docs").unwrap();
        let iter = stmt
            .query_map([], |r| {
                Ok((r.get::<_, i64>(0)? as u64, unblob(&r.get::<_, Vec<u8>>(1)?)))
            })
            .unwrap()
            .map(|x| x.unwrap());
        let _ = topk_exact(&qn, iter, 10);
        lat.push(ms(t.elapsed()));
    }
    let exact_p50 = p50(&mut lat);

    drop(conn);
    let t = Instant::now();
    let conn2 = Connection::open(path).unwrap();
    let _: i64 = conn2
        .query_row("SELECT count(*) FROM docs WHERE category = 'a'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let reopen_ms = ms(t.elapsed());

    Result_ {
        ingest_ms,
        index_ms,
        insert_ms,
        bytes,
        scalar_p50,
        exact_p50,
        ann_p50: None,
        reopen_ms: Some(reopen_ms),
        recall: None,
        rtt: None,
        breakdown: None,
    }
}

// ------------------------------------------------------------- postgres

fn pgvec(v: &[f32]) -> String {
    let mut s = String::with_capacity(v.len() * 8 + 2);
    s.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!("{x}"));
    }
    s.push(']');
    s
}

type BoxErr = Box<dyn std::error::Error>;

fn run_postgres(
    url: &str,
    rows: &[Row],
    queries: &[Vec<f32>],
    dim: usize,
) -> std::result::Result<Result_, BoxErr> {
    let mut cl = Client::connect(url, NoTls)?;
    cl.batch_execute(
        "CREATE EXTENSION IF NOT EXISTS vector;
         DROP TABLE IF EXISTS docs;",
    )?;
    cl.batch_execute(&format!(
        "CREATE TABLE docs (
           id       bigserial PRIMARY KEY,
           category text   NOT NULL,
           score    bigint NOT NULL,
           embed    vector({dim}) NOT NULL
         );"
    ))?;

    // Empty round trip: every measurement below includes it.
    let mut r = Vec::new();
    for _ in 0..50 {
        let t = Instant::now();
        cl.query_one("SELECT 1", &[])?;
        r.push(ms(t.elapsed()));
    }
    let rtt = p50(&mut r);

    // Bulk load with COPY (the recommended path), then the indexes.
    let t = Instant::now();
    {
        let mut w = cl.copy_in("COPY docs (category, score, embed) FROM STDIN")?;
        use std::io::Write;
        let mut buf = String::with_capacity(1 << 20);
        for row in rows {
            buf.push_str(row.category);
            buf.push('\t');
            buf.push_str(&row.score.to_string());
            buf.push('\t');
            buf.push_str(&pgvec(&row.embed));
            buf.push('\n');
            if buf.len() > (1 << 20) {
                w.write_all(buf.as_bytes())?;
                buf.clear();
            }
        }
        w.write_all(buf.as_bytes())?;
        w.finish()?;
    }
    let ingest_ms = ms(t.elapsed());
    // The same HNSW parameters as fenecdb, and the same parallelism: one
    // process per core, the leader included, as fenecdb's build uses every
    // core. pgvector's default of two workers measured 14.7 s against 9.0 s
    // with seven, and made fenecdb look faster at a build it is not faster at.
    let workers = std::thread::available_parallelism()
        .map(|c| c.get().saturating_sub(1))
        .unwrap_or(1);
    cl.batch_execute(&format!(
        "SET max_parallel_maintenance_workers = {workers};"
    ))?;
    let ti = Instant::now();
    cl.batch_execute(
        "CREATE INDEX ON docs (category);
         CREATE INDEX ON docs USING hnsw (embed vector_cosine_ops)
           WITH (m = 16, ef_construction = 200);
         ANALYZE docs;",
    )?;
    let index_ms = ms(ti.elapsed());
    let insert_ms = ms(t.elapsed());

    let bytes: i64 = cl
        .query_one("SELECT pg_total_relation_size('docs')::bigint", &[])?
        .get(0);
    // Most of the difference to fenecdb's file is not MVCC: pgvector's HNSW
    // index keeps a copy of every vector in its own pages, so it reads no
    // heap tuple during a search. The split says so rather than leaving it
    // to a guess.
    let split = cl.query_one(
        "SELECT pg_relation_size('docs')::bigint,
                coalesce(sum(pg_relation_size(i.indexrelid)) FILTER (WHERE am.amname = 'hnsw'), 0)::bigint,
                coalesce(sum(pg_relation_size(i.indexrelid)) FILTER (WHERE am.amname <> 'hnsw'), 0)::bigint
           FROM pg_index i
           JOIN pg_class c ON c.oid = i.indexrelid
           JOIN pg_am am ON am.oid = c.relam
          WHERE i.indrelid = 'docs'::regclass",
        &[],
    )?;
    let mb = |b: i64| b as f64 / 1e6;
    let breakdown = format!(
        "PostgreSQL on disk: heap {:.1} MB, HNSW index {:.1} MB (a copy of every vector), \
         B-trees {:.1} MB",
        mb(split.get(0)),
        mb(split.get(1)),
        mb(split.get(2))
    );

    let mut lat = Vec::new();
    for _ in 0..20 {
        let t = Instant::now();
        let rows_out = cl.query(
            "SELECT id FROM docs WHERE category = $1 AND score > $2",
            &[&"a", &500i64],
        )?;
        lat.push(ms(t.elapsed()));
        assert!(!rows_out.is_empty());
    }
    let scalar_p50 = p50(&mut lat);

    // Exact: index scans are turned off to force a full scan.
    cl.batch_execute("SET enable_indexscan = off; SET enable_indexonlyscan = off;")?;
    let mut lat = Vec::new();
    let mut exact_ids: Vec<Vec<i64>> = Vec::new();
    for q in queries {
        let v = pgvec(q);
        let t = Instant::now();
        let out = cl.query(
            "SELECT id FROM docs ORDER BY embed <=> $1::text::vector LIMIT 10",
            &[&v],
        )?;
        lat.push(ms(t.elapsed()));
        exact_ids.push(out.iter().map(|r| r.get::<_, i64>(0)).collect());
    }
    let exact_p50 = p50(&mut lat);

    // ANN: the same ef_search as fenecdb's default
    cl.batch_execute(&format!(
        "SET enable_indexscan = on; SET enable_indexonlyscan = on; SET hnsw.ef_search = {};",
        VectorIndexSpec::default().ef_search
    ))?;
    let mut lat = Vec::new();
    let mut hits = 0usize;
    let mut total = 0usize;
    for (i, q) in queries.iter().enumerate() {
        let v = pgvec(q);
        let t = Instant::now();
        let out = cl.query(
            "SELECT id FROM docs ORDER BY embed <=> $1::text::vector LIMIT 10",
            &[&v],
        )?;
        lat.push(ms(t.elapsed()));
        let ids: Vec<i64> = out.iter().map(|r| r.get::<_, i64>(0)).collect();
        hits += ids.iter().filter(|id| exact_ids[i].contains(id)).count();
        total += exact_ids[i].len();
    }
    let ann_p50 = p50(&mut lat);

    Ok(Result_ {
        ingest_ms,
        index_ms,
        insert_ms,
        bytes: bytes as u64,
        scalar_p50,
        exact_p50,
        ann_p50: Some(ann_p50),
        reopen_ms: None, // continuously running server: no equivalent
        recall: Some(hits as f64 / total as f64 * 100.0),
        rtt: Some(rtt),
        breakdown: Some(breakdown),
    })
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let n: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(100_000);
    let dim: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(128);

    println!(
        "fenecdb {} vs SQLite {}",
        fenec_core::VERSION,
        rusqlite::version()
    );
    println!("{n} rows × {dim} dims, clustered embedding distribution\n");

    let (rows, queries) = generate(n, dim);
    let dir = std::env::temp_dir();
    let vpath = dir.join("fenecbench.fenec");
    let spath = dir.join("fenecbench.sqlite");

    let v = run_fenecdb(vpath.to_str().unwrap(), &rows, &queries, dim);
    let s = run_sqlite(spath.to_str().unwrap(), &rows, &queries);
    let pg_url = std::env::var("FENECBENCH_PG").unwrap_or_else(|_| {
        "host=127.0.0.1 port=55432 user=postgres password=fenec dbname=fenecbench".into()
    });
    let p = match run_postgres(&pg_url, &rows, &queries, dim) {
        Ok(r) => Some(r),
        Err(e) => {
            eprintln!("(PostgreSQL arm skipped: {e})\n");
            None
        }
    };

    let col = |r: &Option<Result_>, f: &dyn Fn(&Result_) -> String| -> String {
        r.as_ref().map(|x| f(x)).unwrap_or_else(|| "-".into())
    };
    let v = Some(v);
    let s = Some(s);

    let row =
        |label: &str, a: String, b: String, c: String| println!("{label:<30}{a:>13}{b:>13}{c:>15}");
    println!(
        "{:<30}{:>13}{:>13}{:>15}",
        "", "fenecdb", "SQLite", "PG+pgvector"
    );
    println!("{}", "-".repeat(71));
    let secs = |x: &Result_| format!("{:.2} s", x.insert_ms / 1000.0);
    let rate = move |x: &Result_| format!("{:.0}", n as f64 / (x.insert_ms / 1000.0));
    let mb = |x: &Result_| format!("{:.1} MB", x.bytes as f64 / 1e6);
    let sc = |x: &Result_| format!("{:.2} ms", x.scalar_p50);
    let _ = &rate;
    let ex = |x: &Result_| format!("{:.2} ms", x.exact_p50);
    let an = |x: &Result_| {
        x.ann_p50
            .map(|a| format!("{a:.3} ms"))
            .unwrap_or("none".into())
    };
    let rc = |x: &Result_| x.recall.map(|a| format!("{a:.1}%")).unwrap_or("-".into());
    let ro = |x: &Result_| {
        x.reopen_ms
            .map(|a| format!("{a:.1} ms"))
            .unwrap_or("server".into())
    };
    let rt = |x: &Result_| {
        x.rtt
            .map(|a| format!("{a:.3} ms"))
            .unwrap_or("0 (embedded)".into())
    };

    let ing = move |x: &Result_| format!("{:.0} k/s", n as f64 / (x.ingest_ms / 1000.0) / 1000.0);
    let idx = |x: &Result_| format!("{:.2} s", x.index_ms / 1000.0);
    row(
        "data write (no index)",
        col(&v, &ing),
        col(&s, &ing),
        col(&p, &ing),
    );
    row("index build", col(&v, &idx), col(&s, &idx), col(&p, &idx));
    row("total", col(&v, &secs), col(&s, &secs), col(&p, &secs));
    row(
        "  -> rows/s",
        col(&v, &rate),
        col(&s, &rate),
        col(&p, &rate),
    );
    row("on-disk size", col(&v, &mb), col(&s, &mb), col(&p, &mb));
    row(
        "scalar filter (indexed)",
        col(&v, &sc),
        col(&s, &sc),
        col(&p, &sc),
    );
    row(
        "vector top-10, EXACT",
        col(&v, &ex),
        col(&s, &ex),
        col(&p, &ex),
    );
    row(
        "vector top-10, ANN",
        col(&v, &an),
        col(&s, &an),
        col(&p, &an),
    );
    row("  -> recall@10", col(&v, &rc), col(&s, &rc), col(&p, &rc));
    row("reopen", col(&v, &ro), col(&s, &ro), col(&p, &ro));
    row(
        "empty-query round trip",
        col(&v, &rt),
        col(&s, &rt),
        col(&p, &rt),
    );
    println!("{}", "-".repeat(71));

    let v = v.unwrap();
    let s = s.unwrap();

    // The open cost is paid once, the query cost on every query.
    // At which point do the two totals even out?
    let fenec_open = v.reopen_ms.unwrap_or(0.0);
    let sq_open = s.reopen_ms.unwrap_or(0.0);
    let denom = s.exact_p50 - v.ann_p50.unwrap();
    if denom > 0.0 {
        let n_eq = (fenec_open - sq_open) / denom;
        println!(
            "\nOver open + N vector queries in total, fenecdb overtakes SQLite after\n\
             query {:.0} (open {:.0} ms vs {:.0} ms, query {:.2} ms vs {:.2} ms).",
            n_eq.max(0.0).ceil(),
            fenec_open,
            sq_open,
            v.ann_p50.unwrap(),
            s.exact_p50
        );
    }
    println!(
        "\nfenecdb's ANN is {:.0}x faster than its own exact scan.",
        v.exact_p50 / v.ann_p50.unwrap()
    );
    if let Some(p) = &p {
        println!(
            "PostgreSQL measurements include a {:.3} ms TCP round trip; about\n\
             {:.0}% of the ANN time comes from the transport layer.",
            p.rtt.unwrap(),
            p.rtt.unwrap() / p.ann_p50.unwrap() * 100.0
        );
        if let Some(b) = &p.breakdown {
            println!("{b}.");
        }
    }
    let _ = s;

    for p in [vpath, spath] {
        let _ = std::fs::remove_file(&p);
        for s in ["-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{s}", p.to_str().unwrap()));
        }
    }
}
