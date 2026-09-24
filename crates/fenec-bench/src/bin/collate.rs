//! What `order ... collate tr` and `collate und` cost: `make collate-bench`
//!
//! A million Turkish names -- a first name, a second one for a third of
//! them, a surname, and one in twenty in capitals as a registry writes them
//! -- ordered by their bytes and in Turkish, a page of twenty and the whole
//! million, in fenecdb and in PostgreSQL 17 under ICU's `tr-x-icu`. The
//! PostgreSQL arm needs `make pgvector-up` and is skipped without it; its
//! times are the server's own (`EXPLAIN ANALYZE`), in memory (`work_mem` of
//! 1 GB) and on one core, as fenecdb's are.

use fenec_core::prelude::*;
use postgres::{Client, NoTls};
use std::cell::Cell;
use std::time::Instant;

const FIRST: &str = "\
Ahmet Ayşe Aydın Ali Arda Aslı Barış Berk Büşra Burak Can Cem Ceren Çağla Çağrı \
Çiğdem Deniz Derya Doğan Ebru Ece Elif Emre Esra Fatma Gökhan Gönül Gül Hakan Hülya \
Işık Irmak Ilgaz İbrahim İlker İnci İpek İsmail Kâmil Kemal Leyla Mehmet Melek \
Murat Mustafa Nazlı Oğuz Okan Ömer Özge Özgür Pınar Selin Serkan Şahin Şebnem Şule \
Tuğba Uğur Umut Ümit Yağmur Yıldız Zeynep";

const LAST: &str = "\
Yılmaz Kaya Demir Şahin Çelik Yıldız Yıldırım Öztürk Aydın Özdemir Arslan Doğan \
Kılıç Aslan Çetin Kara Koç Kurt Özkan Şimşek Polat Özcan Korkmaz Çakır Erdoğan \
Yavuz Can Acar Şen Aktaş Güler Yalçın Güneş Bozkurt Bulut Keskin Ünal Turan Gül \
Işık";

fn names(n: usize) -> Vec<String> {
    let first: Vec<&str> = FIRST.split(' ').collect();
    let last: Vec<&str> = LAST.split(' ').collect();
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = |m: usize| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        (x % m as u64) as usize
    };
    (0..n)
        .map(|_| {
            let mut s = first[next(first.len())].to_string();
            if next(3) == 0 {
                s.push(' ');
                s.push_str(first[next(first.len())]);
            }
            s.push(' ');
            s.push_str(last[next(last.len())]);
            if next(20) == 0 {
                // Turkish capitals: `i` to `İ`, which `to_uppercase` does not know.
                s = s.replace('i', "İ").to_uppercase();
            }
            s
        })
        .collect()
}

/// The median of five runs after one to warm up, in milliseconds.
fn time(mut f: impl FnMut()) -> f64 {
    f();
    let mut runs: Vec<f64> = (0..5)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    runs.sort_by(f64::total_cmp);
    runs[2]
}

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .map(|a| a.parse().expect("the row count"))
        .unwrap_or(1_000_000);
    let names = names(n);
    let distinct = {
        let mut v = names.clone();
        v.sort_unstable();
        v.dedup();
        v.len()
    };
    println!("{n} names, {distinct} distinct\n");

    // The comparison on its own: the same sort with one comparator and the
    // other, each call counted, and what one comparison costs.
    let copy = time(|| {
        let _ = names.clone();
    });
    println!("sorting {n} strings in memory (the copy, {copy:.1} ms, taken off)");
    let calls = Cell::new(0u64);
    for (label, coll) in [
        ("bytes", None),
        ("tr", Some(Collation::Turkish)),
        ("und", Some(Collation::Root)),
    ] {
        let ms = time(|| {
            let mut v = names.clone();
            calls.set(0);
            v.sort_unstable_by(|a, b| {
                calls.set(calls.get() + 1);
                match coll {
                    Some(c) => c.compare(a, b),
                    None => a.cmp(b),
                }
            });
        }) - copy;
        println!(
            "  {label:<10} {ms:8.1} ms  ({} comparisons, {:.1} ns each)",
            calls.get(),
            ms * 1e6 / calls.get() as f64
        );
    }
    println!();

    let mut db = Database::new();
    let run = |db: &mut Database, sql: &str, params: &[Value]| {
        db.execute_with(&fenec_ql::parse_one(sql).unwrap(), params)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    };
    run(&mut db, "create collection people (name text)", &[]);
    for chunk in names.chunks(2_000) {
        db.execute(&Statement::Put {
            collection: "people".into(),
            docs: chunk
                .iter()
                .map(|s| vec![("name".into(), Expr::Lit(Value::Text(s.clone())))])
                .collect(),
        })
        .unwrap();
    }
    let all = n.saturating_sub(20);
    let queries = [
        ("a page, bytes", "order name limit 20".to_string()),
        ("a page, tr", "order name collate tr limit 20".to_string()),
        ("a page, und", "order name collate und limit 20".to_string()),
        ("all, bytes", format!("order name offset {all} limit 20")),
        (
            "all, tr",
            format!("order name collate tr offset {all} limit 20"),
        ),
        (
            "all, und",
            format!("order name collate und offset {all} limit 20"),
        ),
    ];
    println!("fenecdb, in process");
    for (label, q) in &queries {
        let stmt = fenec_ql::parse_one(&format!("get people select name {q}")).unwrap();
        let ms = time(|| {
            db.query(&stmt, &[]).unwrap();
        });
        println!("  {label:<14} {ms:8.1} ms");
    }

    let url = std::env::var("FENECBENCH_PG").unwrap_or_else(|_| {
        "host=127.0.0.1 port=55432 user=postgres password=fenec dbname=fenecbench".into()
    });
    match postgres_arm(&url, &names) {
        Ok(()) => {}
        Err(e) => println!("\n(PostgreSQL arm skipped: {e})"),
    }
}

// The prelude's `Result` is the engine's.
type Outcome<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn postgres_arm(url: &str, names: &[String]) -> Outcome<()> {
    let mut cl = Client::connect(url, NoTls)?;
    cl.batch_execute(
        "DROP TABLE IF EXISTS collate_people;
         CREATE TABLE collate_people (name text NOT NULL);",
    )?;
    {
        let mut w = cl.copy_in("COPY collate_people (name) FROM STDIN")?;
        use std::io::Write;
        for chunk in names.chunks(10_000) {
            let mut buf = String::new();
            for s in chunk {
                buf.push_str(s);
                buf.push('\n');
            }
            w.write_all(buf.as_bytes())?;
        }
        w.finish()?;
    }
    cl.batch_execute("ANALYZE collate_people; SET work_mem = '1GB';")?;
    let version: String = cl.query_one("SHOW server_version", &[])?.get(0);
    let all = names.len().saturating_sub(20);
    println!("\nPostgreSQL {version}, the server's execution time");
    for (label, q) in [
        (
            "a page, C",
            "ORDER BY name COLLATE \"C\" LIMIT 20".to_string(),
        ),
        (
            "a page, tr",
            "ORDER BY name COLLATE \"tr-x-icu\" LIMIT 20".to_string(),
        ),
        (
            "all, C",
            format!("ORDER BY name COLLATE \"C\" OFFSET {all} LIMIT 20"),
        ),
        (
            "all, tr",
            format!("ORDER BY name COLLATE \"tr-x-icu\" OFFSET {all} LIMIT 20"),
        ),
    ] {
        let sql = format!("EXPLAIN (ANALYZE, TIMING OFF) SELECT name FROM collate_people {q}");
        let exec = server_time(&mut cl, &sql)?;
        println!("  {label:<14} {exec:8.1} ms");
    }
    cl.batch_execute("DROP TABLE collate_people;")?;
    Ok(())
}

/// The median of five `Execution Time`s, after one run to warm up.
fn server_time(cl: &mut Client, sql: &str) -> Outcome<f64> {
    let mut runs = Vec::new();
    for i in 0..6 {
        let mut t = None;
        for row in cl.query(sql, &[])? {
            let line: String = row.get(0);
            if let Some(ms) = line.strip_prefix("Execution Time: ") {
                t = Some(ms.trim_end_matches(" ms").parse::<f64>()?);
            }
        }
        if i > 0 {
            runs.push(t.ok_or("no execution time in the plan")?);
        }
    }
    runs.sort_by(f64::total_cmp);
    Ok(runs[2])
}
