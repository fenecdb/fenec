//! What one request costs over the wire, fenec-server against PostgreSQL:
//! `make requests-bench`.
//!
//! fenec-server is started here over a file of 10 000 rows, a 128-dim vector
//! each, with its HTTP on loopback; PostgreSQL + pgvector is the container
//! `make pgvector-up` starts, and is skipped without it. Four requests -- a
//! row by id, a filter with a limit, a `near` top ten and a put of a row
//! without a vector -- each asked by one client at a time (the round trip's
//! p50 and p99) and by eight at once (requests a second):
//!
//!   * fenec-server: `POST /query` with the parameters beside the text, on a
//!     connection a client keeps alive ([`http::Http`]);
//!   * PostgreSQL over its extended protocol, the statement prepared once
//!     and bound for each request, as a driver does;
//!   * PostgreSQL over its simple protocol, the literals in the text.
//!
//! PostgreSQL answers from inside Docker's virtual machine, so its figures
//! carry that network: the round trip of the least each can be asked --
//! fenec-server's `GET /`, PostgreSQL's empty query -- is printed beside
//! them. Its writes commit with `synchronous_commit = off`, as
//! fenec-server's reach the disk within `--sync 250`.

#[path = "../http.rs"]
mod http;

use postgres::{Client, NoTls, SimpleQueryMessage};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use fenec_core::prelude::*;

const ROWS: usize = 10_000;
const DIM: usize = 128;
/// Requests timed one at a time, a case and a way of asking.
const ONE: usize = 3_000;
const CLIENTS: usize = 8;
const RUN: Duration = Duration::from_secs(2);
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

fn vectors() -> Vec<Vec<f32>> {
    let mut rng = Rng(7);
    let centres: Vec<Vec<f32>> = (0..16)
        .map(|_| (0..DIM).map(|_| rng.f32()).collect())
        .collect();
    (0..ROWS)
        .map(|i| {
            centres[i % 16]
                .iter()
                .map(|c| c + 0.2 * rng.f32())
                .collect()
        })
        .collect()
}

fn literal(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|x| format!("{x}")).collect();
    format!("[{}]", parts.join(","))
}

/// A request's parameters, drawn afresh each time.
#[derive(Clone, Copy, PartialEq)]
enum Case {
    ById,
    Filter,
    Near,
    Put,
}

const CASES: [(Case, &str); 4] = [
    (Case::ById, "a row by id"),
    (Case::Filter, "filter, limit 10"),
    (Case::Near, "near, top 10"),
    (Case::Put, "put a row"),
];

enum Param {
    Int(i64),
    Text(String),
}

fn params(case: Case, rng: &mut Rng, queries: &[String], n: u64) -> Vec<Param> {
    match case {
        Case::ById => vec![Param::Int((rng.next() % ROWS as u64) as i64 + 1)],
        Case::Filter => vec![
            Param::Text(CATEGORIES[(rng.next() % 4) as usize].into()),
            Param::Int((rng.next() % 1000) as i64),
        ],
        Case::Near => vec![Param::Text(
            queries[(rng.next() % queries.len() as u64) as usize].clone(),
        )],
        // PostgreSQL's score is an `int4`.
        Case::Put => vec![Param::Text("x".into()), Param::Int((n % 1_000_000) as i64)],
    }
}

// ------------------------------------------------------------- engines

/// fenec-server's statement for each case, its parameters `$n`.
fn fenec_text(case: Case) -> &'static str {
    match case {
        Case::ById => "get docs select id, category, score where id = $1",
        Case::Filter => "get docs select id where category = $1 and score > $2 limit 10",
        Case::Near => "get docs select id near embed $1 limit 10",
        Case::Put => "put docs {category: $1, score: $2}",
    }
}

/// PostgreSQL's statement for each case: the text with `$n`, and the text
/// with the literals in for the simple protocol.
fn pg_text(case: Case) -> &'static str {
    match case {
        Case::ById => "SELECT id, category, score FROM docs WHERE id = $1",
        Case::Filter => "SELECT id FROM docs WHERE category = $1 AND score > $2 LIMIT 10",
        Case::Near => "SELECT id FROM docs ORDER BY embed <=> $1::text::vector LIMIT 10",
        Case::Put => "INSERT INTO docs (category, score) VALUES ($1, $2)",
    }
}

fn pg_inline(case: Case, p: &[Param]) -> String {
    let (a, b) = (lit(&p[0]), p.get(1).map(lit));
    match case {
        Case::ById => format!("SELECT id, category, score FROM docs WHERE id = {a}"),
        Case::Filter => format!(
            "SELECT id FROM docs WHERE category = {a} AND score > {} LIMIT 10",
            b.unwrap()
        ),
        Case::Near => format!(
            "SELECT id FROM docs ORDER BY embed <=> '{}' LIMIT 10",
            raw(&p[0])
        ),
        Case::Put => format!(
            "INSERT INTO docs (category, score) VALUES ({a}, {})",
            b.unwrap()
        ),
    }
}

/// A literal in SQL's text.
fn lit(p: &Param) -> String {
    match p {
        Param::Int(i) => i.to_string(),
        Param::Text(s) => format!("'{s}'"),
    }
}

fn raw(p: &Param) -> &str {
    match p {
        Param::Text(s) => s,
        Param::Int(_) => unreachable!(),
    }
}

// ------------------------------------------------------------ requests

/// One client's way of asking: `ask` sends a request and waits for the
/// whole answer.
trait Asker: Send {
    fn ask(&mut self, case: Case, p: &[Param]);
}

/// PostgreSQL's extended protocol through the `postgres` crate, each
/// case's statement prepared once.
struct Extended {
    client: Client,
    stmts: Vec<postgres::Statement>,
}

impl Extended {
    fn new(url: &str) -> Extended {
        let mut client = pg_client(url);
        let stmts = CASES
            .iter()
            .map(|(c, _)| client.prepare(pg_text(*c)).unwrap())
            .collect();
        Extended { client, stmts }
    }
}

impl Asker for Extended {
    fn ask(&mut self, case: Case, p: &[Param]) {
        let i = CASES.iter().position(|(c, _)| *c == case).unwrap();
        let stmt = &self.stmts[i];
        // The table's id and score are `int4`.
        let values: Vec<Box<dyn postgres::types::ToSql + Sync>> = p
            .iter()
            .map(|x| -> Box<dyn postgres::types::ToSql + Sync> {
                match x {
                    Param::Int(i) => Box::new(*i as i32),
                    Param::Text(s) => Box::new(s.clone()),
                }
            })
            .collect();
        let refs: Vec<&(dyn postgres::types::ToSql + Sync)> =
            values.iter().map(|b| b.as_ref()).collect();
        match case {
            Case::Put => {
                self.client.execute(stmt, &refs).unwrap();
            }
            _ => {
                self.client.query(stmt, &refs).unwrap();
            }
        }
    }
}

/// PostgreSQL's simple protocol, the literals in the text.
struct Simple {
    client: Client,
}

impl Asker for Simple {
    fn ask(&mut self, case: Case, p: &[Param]) {
        let out = self.client.simple_query(&pg_inline(case, p)).unwrap();
        assert!(out
            .iter()
            .any(|m| matches!(m, SimpleQueryMessage::CommandComplete(_))));
    }
}

fn pg_client(url: &str) -> Client {
    let mut client = Client::connect(url, NoTls).unwrap();
    client
        .batch_execute("SET synchronous_commit = off; SET hnsw.ef_search = 100")
        .unwrap();
    client
}

/// `POST /query`, the parameters beside the text, one connection kept alive.
struct Http(http::Http);

impl Asker for Http {
    fn ask(&mut self, case: Case, p: &[Param]) {
        let params: Vec<String> = p
            .iter()
            .map(|x| match x {
                Param::Int(i) => i.to_string(),
                // A vector goes as the array it is, a text as a string.
                Param::Text(s) if case == Case::Near => s.clone(),
                Param::Text(s) => format!("\"{s}\""),
            })
            .collect();
        self.0.query(fenec_text(case), &params.join(","));
    }
}

// ---------------------------------------------------------- measuring

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[((v.len() - 1) as f64 * p) as usize]
}

/// p50 and p99 in ms of `ONE` requests asked one at a time.
fn one_at_a_time(a: &mut dyn Asker, case: Case, queries: &[String], seed: u64) -> (f64, f64) {
    let mut rng = Rng(seed);
    for n in 0..200 {
        a.ask(case, &params(case, &mut rng, queries, n));
    }
    let mut lat = Vec::with_capacity(ONE);
    for n in 0..ONE as u64 {
        let p = params(case, &mut rng, queries, 1_000_000 + n);
        let t = Instant::now();
        a.ask(case, &p);
        lat.push(t.elapsed().as_secs_f64() * 1e3);
    }
    (pct(&mut lat, 0.5), pct(&mut lat, 0.99))
}

/// Requests a second from `CLIENTS` clients asking at once for `RUN`.
fn at_once(make: &(dyn Fn() -> Box<dyn Asker> + Sync), case: Case, queries: &[String]) -> f64 {
    let done = AtomicU64::new(0);
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        for c in 0..CLIENTS as u64 {
            let (done, stop) = (&done, &stop);
            s.spawn(move || {
                let mut a = make();
                let mut rng = Rng(1000 + c);
                let mut n = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    a.ask(case, &params(case, &mut rng, queries, (c << 40) + n));
                    n += 1;
                }
                done.fetch_add(n, Ordering::Relaxed);
            });
        }
        std::thread::sleep(RUN);
        stop.store(true, Ordering::Relaxed);
    });
    done.load(Ordering::Relaxed) as f64 / RUN.as_secs_f64()
}

// ------------------------------------------------------------- set up

fn fenec_file(path: &Path, vecs: &[Vec<f32>]) {
    let _ = std::fs::remove_file(path);
    let mut db = fenec_core::fs::open(path).unwrap();
    db.execute(
        &fenec_ql::parse_one(
            "create collection docs (category text @hash, score int, embed vector<128> @hnsw(cosine))",
        )
        .unwrap(),
    )
    .unwrap();
    for (b, chunk) in vecs.chunks(1000).enumerate() {
        let docs = chunk
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let n = b * 1000 + i;
                vec![
                    (
                        "category".to_string(),
                        Expr::Lit(Value::Text(CATEGORIES[n % 4].into())),
                    ),
                    (
                        "score".to_string(),
                        Expr::Lit(Value::Int((n % 1000) as i64)),
                    ),
                    ("embed".to_string(), Expr::Lit(Value::Vector(v.clone()))),
                ]
            })
            .collect();
        db.execute(&Statement::Put {
            collection: "docs".into(),
            docs,
            insert: false,
            if_absent: false,
            require: None,
        })
        .unwrap();
    }
    db.checkpoint().unwrap();
}

fn pg_setup(url: &str, vecs: &[Vec<f32>]) -> bool {
    let Ok(mut c) = Client::connect(url, NoTls) else {
        return false;
    };
    c.batch_execute(
        "CREATE EXTENSION IF NOT EXISTS vector;
         DROP TABLE IF EXISTS docs;
         CREATE TABLE docs (id serial PRIMARY KEY, category text NOT NULL, score int NOT NULL, embed vector(128));",
    )
    .unwrap();
    let mut w = c
        .copy_in("COPY docs (category, score, embed) FROM STDIN")
        .unwrap();
    for (n, v) in vecs.iter().enumerate() {
        writeln!(w, "{}\t{}\t{}", CATEGORIES[n % 4], n % 1000, literal(v)).unwrap();
    }
    w.finish().unwrap();
    c.batch_execute(
        "CREATE INDEX ON docs (category);
         CREATE INDEX ON docs USING hnsw (embed vector_cosine_ops) WITH (m = 16, ef_construction = 200);
         ANALYZE docs;",
    )
    .unwrap();
    true
}

/// p50 in ms of the least a server can be asked, `ONE` times.
fn round_trip(mut ask: impl FnMut()) -> f64 {
    let mut lat: Vec<f64> = (0..ONE)
        .map(|_| {
            let t = Instant::now();
            ask();
            t.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    pct(&mut lat, 0.5)
}

fn main() {
    let vecs = vectors();
    let queries: Vec<String> = (0..100).map(|i| literal(&vecs[i * 97])).collect();
    let dir = std::env::temp_dir().join(format!("fenecbench-requests-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("requests.fenec");
    fenec_file(&file, &vecs);
    let http_port = http::free_port();
    let _server = http::start_fenec(&file, http_port, "requests-bench", &[]);
    let http_addr = format!("127.0.0.1:{http_port}");
    let pg_url = std::env::var("FENECBENCH_PG").unwrap_or_else(|_| {
        "host=127.0.0.1 port=55432 user=postgres password=fenec dbname=fenecbench".into()
    });
    let pg = pg_setup(&pg_url, &vecs);

    println!("{ROWS} rows x {DIM} dims; one client: round trip p50 / p99 in ms; {CLIENTS} clients: requests a second");
    let mut empty = http::Http::connect(&http_addr);
    print!(
        "the least request's round trip: fenec-server's GET / {:.3} ms",
        round_trip(|| {
            empty.get("/");
        })
    );
    if pg {
        let mut c = Client::connect(&pg_url, NoTls).unwrap();
        print!(
            ", PostgreSQL's empty query {:.3} ms (Docker's network)",
            round_trip(|| {
                c.simple_query("").unwrap();
            })
        );
    }
    println!("\n");
    println!(
        "{:<20} {:>22} {:>22} {:>22}",
        "",
        "fenec-server, HTTP",
        if pg { "PostgreSQL, extended" } else { "" },
        if pg { "PostgreSQL, simple" } else { "" }
    );
    for (case, name) in CASES {
        let mut cells = Vec::new();
        for way in 0..3 {
            if way > 0 && !pg {
                continue;
            }
            let (addr, u) = (http_addr.clone(), pg_url.clone());
            let make = move || -> Box<dyn Asker> {
                match way {
                    0 => Box::new(Http(http::Http::connect(&addr))),
                    1 => Box::new(Extended::new(&u)),
                    _ => Box::new(Simple {
                        client: pg_client(&u),
                    }),
                }
            };
            let mut one = make();
            let (p50, p99) = one_at_a_time(one.as_mut(), case, &queries, 42);
            drop(one);
            let rate = at_once(&make, case, &queries);
            cells.push(format!("{p50:.3} / {p99:.3}  {:>7.0}/s", rate));
        }
        println!(
            "{:<20} {:>22} {:>22} {:>22}",
            name,
            cells[0],
            cells.get(1).map(String::as_str).unwrap_or(""),
            cells.get(2).map(String::as_str).unwrap_or("")
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
