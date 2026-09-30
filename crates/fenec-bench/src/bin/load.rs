//! What loading rows costs, each way a client can send them: `make
//! load-bench`.
//!
//! 100 000 rows of a text, an int and a 128-dim vector, into an empty
//! collection, with the vector's HNSW index kept as they land and without
//! one. fenec-pg is started here over an empty file, with its pg wire and
//! HTTP on loopback:
//!
//!   * in process: `put` of 1 000 rows at a time on the `Database` itself,
//!     what the rest can at most reach;
//!   * pg wire, simple protocol: the text `put docs [{..}, ..]` of 1 000
//!     rows at a time, the literals in it;
//!   * pg wire, extended protocol: in a transaction, `put docs {category:
//!     $1, score: $2, embed: $3}` bound once a row and run, a Sync every
//!     1 000 rows -- as a driver's `executemany` pipelines them;
//!   * pg wire, `COPY docs (..) FROM STDIN`: the rows as text, through the
//!     `postgres` crate's `copy_in`, as it is sent to PostgreSQL below;
//!   * HTTP: `POST /docs` with a JSON array of 1 000 rows;
//!   * HTTP: `POST /batch` with 1 000 lines, each a `POST /query` body.
//!
//! Then the rows are read back whole by `COPY docs TO STDOUT`, in text and
//! in binary, through the `postgres` crate's `copy_out`, from fenec-pg and
//! from PostgreSQL.
//!
//! PostgreSQL + pgvector, the container `make pgvector-up` starts (skipped
//! without it), is sent the same rows by `COPY ... FROM STDIN` and by
//! `INSERT` of 1 000 rows a statement, with `synchronous_commit = off`, as
//! fenec-pg's writes reach the disk within `--sync 250`; it answers from
//! inside Docker's virtual machine, whose network carries every byte.

#[path = "../wire.rs"]
mod wire;

use fenec_core::prelude::*;
use postgres::{Client, NoTls};
use std::io::Write as _;
use std::time::Instant;

const ROWS: usize = 100_000;
const DIM: usize = 128;
const BATCH: usize = 1_000;
const CATEGORIES: [&str; 4] = ["a", "b", "c", "d"];

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn f32(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32 - 0.5
    }
}

struct Row {
    category: &'static str,
    score: i64,
    embed: Vec<f32>,
    /// The vector as the text a client writes it, `[..]`.
    text: String,
}

fn rows() -> Vec<Row> {
    let mut rng = Rng(11);
    let centres: Vec<Vec<f32>> = (0..16)
        .map(|_| (0..DIM).map(|_| rng.f32()).collect())
        .collect();
    (0..ROWS)
        .map(|i| {
            let embed: Vec<f32> = centres[i % 16]
                .iter()
                .map(|c| c + 0.2 * rng.f32())
                .collect();
            let parts: Vec<String> = embed.iter().map(|x| format!("{x}")).collect();
            Row {
                category: CATEGORIES[i % 4],
                score: (i % 1000) as i64,
                text: format!("[{}]", parts.join(",")),
                embed,
            }
        })
        .collect()
}

fn schema(name: &str, indexed: bool) -> String {
    format!(
        "create collection {name} (category text @hash, score int, embed vector<{DIM}>{})",
        if indexed { " @hnsw(cosine)" } else { "" }
    )
}

fn rate(t: Instant) -> String {
    format!("{:>9.0} rows/s", ROWS as f64 / t.elapsed().as_secs_f64())
}

// -------------------------------------------------------------- fenecdb

fn in_process(rows: &[Row], indexed: bool) -> String {
    let mut db = Database::new();
    db.execute(&fenec_ql::parse_one(&schema("docs", indexed)).unwrap())
        .unwrap();
    let t = Instant::now();
    for chunk in rows.chunks(BATCH) {
        let docs = chunk
            .iter()
            .map(|r| {
                vec![
                    (
                        "category".to_string(),
                        Expr::Lit(Value::Text(r.category.into())),
                    ),
                    ("score".to_string(), Expr::Lit(Value::Int(r.score))),
                    (
                        "embed".to_string(),
                        Expr::Lit(Value::Vector(r.embed.clone())),
                    ),
                ]
            })
            .collect();
        db.execute(&Statement::Put {
            collection: "docs".into(),
            docs,
            insert: false,
        })
        .unwrap();
    }
    rate(t)
}

fn pg_simple(addr: &str, name: &str, rows: &[Row]) -> String {
    let mut c = wire::Wire::connect(addr);
    let t = Instant::now();
    for chunk in rows.chunks(BATCH) {
        let body: Vec<String> = chunk
            .iter()
            .map(|r| {
                format!(
                    "{{category: \"{}\", score: {}, embed: {}}}",
                    r.category, r.score, r.text
                )
            })
            .collect();
        c.simple(&format!("put {name} [{}]", body.join(", ")));
    }
    rate(t)
}

fn pg_extended(addr: &str, name: &str, rows: &[Row]) -> String {
    let mut c = wire::Wire::connect(addr);
    c.prepare(
        "put",
        &format!("put {name} {{category: $1, score: $2, embed: $3}}"),
    );
    let t = Instant::now();
    c.simple("BEGIN");
    for chunk in rows.chunks(BATCH) {
        for r in chunk {
            let score = r.score.to_string();
            c.bind_execute("put", &[r.category, &score, &r.text]);
        }
        c.sync();
    }
    c.simple("COMMIT");
    rate(t)
}

fn pg_copy(addr: &str, name: &str, rows: &[Row]) -> String {
    let (host, port) = addr.split_once(':').unwrap();
    let url = format!("host={host} port={port} user=fenec dbname=fenec");
    let mut c = Client::connect(&url, NoTls).unwrap();
    postgres_copy(&mut c, name, rows)
}

fn http_array(addr: &str, name: &str, rows: &[Row]) -> String {
    let mut c = wire::Http::connect(addr);
    let path = format!("/{name}");
    let t = Instant::now();
    for chunk in rows.chunks(BATCH) {
        let body: Vec<String> = chunk
            .iter()
            .map(|r| {
                format!(
                    "{{\"category\":\"{}\",\"score\":{},\"embed\":{}}}",
                    r.category, r.score, r.text
                )
            })
            .collect();
        c.post(
            &path,
            "application/json",
            format!("[{}]", body.join(",")).as_bytes(),
        );
    }
    rate(t)
}

fn http_batch(addr: &str, name: &str, rows: &[Row]) -> String {
    let mut c = wire::Http::connect(addr);
    let t = Instant::now();
    for chunk in rows.chunks(BATCH) {
        let lines: Vec<String> = chunk
            .iter()
            .map(|r| {
                format!(
                    "{{\"query\":\"put {name} {{category: $1, score: $2, embed: $3}}\",\"params\":[\"{}\",{},{}]}}",
                    r.category, r.score, r.text
                )
            })
            .collect();
        c.post(
            "/batch",
            "application/x-ndjson",
            lines.join("\n").as_bytes(),
        );
    }
    rate(t)
}

// ----------------------------------------------------------- PostgreSQL

fn pg_table(c: &mut Client, name: &str, indexed: bool) {
    c.batch_execute(&format!(
        "DROP TABLE IF EXISTS {name};
         CREATE TABLE {name} (id serial PRIMARY KEY, category text, score int, embed vector({DIM}));
         CREATE INDEX ON {name} (category);
         {}",
        if indexed {
            format!("CREATE INDEX ON {name} USING hnsw (embed vector_cosine_ops) WITH (m = 16, ef_construction = 200);")
        } else {
            String::new()
        }
    ))
    .unwrap();
}

fn postgres_copy(c: &mut Client, name: &str, rows: &[Row]) -> String {
    let t = Instant::now();
    let mut w = c
        .copy_in(&format!("COPY {name} (category, score, embed) FROM STDIN"))
        .unwrap();
    for r in rows {
        writeln!(w, "{}\t{}\t{}", r.category, r.score, r.text).unwrap();
    }
    w.finish().unwrap();
    rate(t)
}

fn postgres_insert(c: &mut Client, name: &str, rows: &[Row]) -> String {
    let t = Instant::now();
    for chunk in rows.chunks(BATCH) {
        let values: Vec<String> = chunk
            .iter()
            .map(|r| format!("('{}', {}, '{}')", r.category, r.score, r.text))
            .collect();
        c.batch_execute(&format!(
            "INSERT INTO {name} (category, score, embed) VALUES {}",
            values.join(", ")
        ))
        .unwrap();
    }
    rate(t)
}

/// `COPY <name> TO STDOUT` read to its end, in text or in binary: its rows
/// a second.
fn copy_out(c: &mut Client, name: &str, binary: bool) -> String {
    use std::io::Read as _;
    let t = Instant::now();
    let mut bytes = Vec::new();
    let with = if binary { " (FORMAT binary)" } else { "" };
    c.copy_out(&format!(
        "COPY {name} (category, score, embed) TO STDOUT{with}"
    ))
    .unwrap()
    .read_to_end(&mut bytes)
    .unwrap();
    // Text: a line a row. Binary: a row's 3 columns, then its cells, each
    // behind its length.
    let rows = match binary {
        false => bytes.iter().filter(|&&b| b == b'\n').count(),
        true => {
            let (mut at, mut rows) = (19, 0);
            while i16::from_be_bytes([bytes[at], bytes[at + 1]]) == 3 {
                at += 2;
                for _ in 0..3 {
                    let len = i32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
                    at += 4 + len.max(0) as usize;
                }
                rows += 1;
            }
            rows
        }
    };
    assert_eq!(rows, ROWS, "{name}");
    let secs = t.elapsed().as_secs_f64();
    format!("{:.1}k rows/s", rows as f64 / secs / 1e3)
}

fn main() {
    let rows = rows();
    let dir = std::env::temp_dir().join(format!("fenecbench-load-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("load.fenec");
    let _ = std::fs::remove_file(&file);
    let (pg_port, http_port) = (wire::free_port(), wire::free_port());
    let _server = wire::start_fenec(&file, pg_port, http_port, "load-bench");
    let (pg_addr, http_addr) = (
        format!("127.0.0.1:{pg_port}"),
        format!("127.0.0.1:{http_port}"),
    );
    let pg_url = std::env::var("FENECBENCH_PG").unwrap_or_else(|_| {
        "host=127.0.0.1 port=55432 user=postgres password=fenec dbname=fenecbench".into()
    });
    let mut postgres = Client::connect(&pg_url, NoTls).ok();
    if let Some(c) = postgres.as_mut() {
        c.batch_execute("CREATE EXTENSION IF NOT EXISTS vector; SET synchronous_commit = off")
            .unwrap();
    }

    println!("{ROWS} rows of a text, an int and a vector<{DIM}>, {BATCH} a request\n");
    println!("{:<34} {:>16} {:>16}", "", "HNSW kept", "no vector index");
    let mut admin = wire::Wire::connect(&pg_addr);
    let mut n = 0;
    let mut table = |admin: &mut wire::Wire, indexed: bool| {
        n += 1;
        let name = format!("docs{n}");
        admin.simple(&schema(&name, indexed));
        name
    };
    type Way<'a> = (&'a str, &'a dyn Fn(&str, &[Row]) -> String);
    let ways: [Way; 5] = [
        ("pg wire, simple: put of 1 000", &|name, rows| {
            pg_simple(&pg_addr, name, rows)
        }),
        ("pg wire, extended, a transaction", &|name, rows| {
            pg_extended(&pg_addr, name, rows)
        }),
        ("pg wire, COPY", &|name, rows| pg_copy(&pg_addr, name, rows)),
        ("HTTP, POST /docs, an array", &|name, rows| {
            http_array(&http_addr, name, rows)
        }),
        ("HTTP, POST /batch", &|name, rows| {
            http_batch(&http_addr, name, rows)
        }),
    ];
    println!(
        "{:<34} {:>16} {:>16}",
        "in process, put of 1 000",
        in_process(&rows, true),
        in_process(&rows, false)
    );
    for (what, way) in ways {
        let with = way(&table(&mut admin, true), &rows);
        let without = way(&table(&mut admin, false), &rows);
        println!("{what:<34} {with:>16} {without:>16}");
    }
    // Read back from a collection loaded with no vector index: the second.
    let (host, port) = pg_addr.split_once(':').unwrap();
    let mut fenec = Client::connect(
        &format!("host={host} port={port} user=fenec dbname=fenec"),
        NoTls,
    )
    .unwrap();
    println!(
        "\n{:<34} {:>16}",
        "read back, COPY TO STDOUT",
        copy_out(&mut fenec, "docs2", false)
    );
    println!(
        "{:<34} {:>16}",
        "read back, COPY TO, binary",
        copy_out(&mut fenec, "docs2", true)
    );
    if let Some(c) = postgres.as_mut() {
        let mut row = |what: &str, way: &dyn Fn(&mut Client, &str, &[Row]) -> String| {
            pg_table(c, "load_hnsw", true);
            let with = way(c, "load_hnsw", &rows);
            pg_table(c, "load_plain", false);
            let without = way(c, "load_plain", &rows);
            println!("{what:<34} {with:>16} {without:>16}");
        };
        println!();
        row("PostgreSQL, COPY", &postgres_copy);
        row("PostgreSQL, INSERT of 1 000", &postgres_insert);
        println!(
            "{:<34} {:>16}",
            "PostgreSQL, COPY TO STDOUT",
            copy_out(c, "load_plain", false)
        );
        println!(
            "{:<34} {:>16}",
            "PostgreSQL, COPY TO, binary",
            copy_out(c, "load_plain", true)
        );
        c.batch_execute("DROP TABLE IF EXISTS load_hnsw; DROP TABLE IF EXISTS load_plain")
            .unwrap();
    }
    let _ = std::fs::remove_dir_all(&dir);
}
