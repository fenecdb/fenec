//! What one request costs over the wire, fenec-pg against PostgreSQL:
//! `make requests-bench`.
//!
//! fenec-pg is started here over a file of 10 000 rows, a 128-dim vector
//! each, with its pg wire and HTTP on loopback; PostgreSQL + pgvector is the
//! container `make pgvector-up` starts, and is skipped without it. Four
//! requests -- a row by id, a filter with a limit, a `near` top ten and a
//! put of a row without a vector -- each asked by one client at a time (the
//! round trip's p50 and p99) and by eight at once (requests a second):
//!
//!   * over the pg wire's extended protocol, the statement prepared once
//!     and bound for each request, as a driver does;
//!   * over its simple protocol, the literals in the text;
//!   * over HTTP (fenec-pg alone), `POST /query` with the parameters beside
//!     the text, on a kept-alive connection.
//!
//! PostgreSQL answers from inside Docker's virtual machine, so its figures
//! carry that network: its empty query's round trip is printed beside them.
//! Its writes commit with `synchronous_commit = off`, as fenec-pg's reach
//! the disk within `--sync 250`.

use postgres::{Client, NoTls, SimpleQueryMessage};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use fenec_core::prelude::*;

const ROWS: usize = 10_000;
const DIM: usize = 128;
/// Requests timed one at a time, a case and a protocol.
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

/// One engine's statements for each case: the text with `$n`, and the text
/// with the literals in for the simple protocol.
trait Engine: Sync {
    fn text(&self, case: Case) -> &'static str;
    fn inline(&self, case: Case, p: &[Param]) -> String;
}

struct Fenec;
impl Engine for Fenec {
    fn text(&self, case: Case) -> &'static str {
        match case {
            Case::ById => "get docs select id, category, score where id = $1",
            Case::Filter => "get docs select id where category = $1 and score > $2 limit 10",
            Case::Near => "get docs select id near embed $1 limit 10",
            Case::Put => "put docs {category: $1, score: $2}",
        }
    }
    fn inline(&self, case: Case, p: &[Param]) -> String {
        let (a, b) = (lit(&p[0], '"'), p.get(1).map(|x| lit(x, '"')));
        match case {
            Case::ById => format!("get docs select id, category, score where id = {a}"),
            Case::Filter => format!(
                "get docs select id where category = {a} and score > {} limit 10",
                b.unwrap()
            ),
            Case::Near => format!("get docs select id near embed {} limit 10", raw(&p[0])),
            Case::Put => format!("put docs {{category: {a}, score: {}}}", b.unwrap()),
        }
    }
}

struct Pg;
impl Engine for Pg {
    fn text(&self, case: Case) -> &'static str {
        match case {
            Case::ById => "SELECT id, category, score FROM docs WHERE id = $1",
            Case::Filter => "SELECT id FROM docs WHERE category = $1 AND score > $2 LIMIT 10",
            Case::Near => "SELECT id FROM docs ORDER BY embed <=> $1::text::vector LIMIT 10",
            Case::Put => "INSERT INTO docs (category, score) VALUES ($1, $2)",
        }
    }
    fn inline(&self, case: Case, p: &[Param]) -> String {
        let (a, b) = (lit(&p[0], '\''), p.get(1).map(|x| lit(x, '\'')));
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
}

/// A literal in the text: FenecQL quotes a string in `"`, SQL in `'`.
fn lit(p: &Param, quote: char) -> String {
    match p {
        Param::Int(i) => i.to_string(),
        Param::Text(s) => format!("{quote}{s}{quote}"),
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
            .map(|(c, _)| client.prepare(Pg.text(*c)).unwrap())
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
        let out = self.client.simple_query(&Pg.inline(case, p)).unwrap();
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

/// fenec-pg over the pg wire, written by hand, text both ways and each
/// case's statement parsed once -- as psycopg asks. The `postgres` crate
/// binds and reads in binary, and looks up in `pg_type` any type it does
/// not know: fenec-pg sends a parameter it has not typed as OID 0, and the
/// crate's statement for the lookup has its own parameter come back as 0,
/// which it looks up the same way until the stack runs out.
struct Wire {
    w: TcpStream,
    r: BufReader<TcpStream>,
    extended: bool,
    out: Vec<u8>,
    body: Vec<u8>,
}

impl Wire {
    fn new(addr: &str, extended: bool) -> Wire {
        let w = TcpStream::connect(addr).unwrap();
        w.set_nodelay(true).unwrap();
        let r = BufReader::new(w.try_clone().unwrap());
        let mut c = Wire {
            w,
            r,
            extended,
            out: Vec::new(),
            body: Vec::new(),
        };
        // The startup packet: its length, protocol 3.0, the user, the
        // database.
        let mut startup = 196_608i32.to_be_bytes().to_vec();
        for s in ["user", "fenec", "database", "fenec", ""] {
            cstr(&mut startup, s);
        }
        let mut packet = ((startup.len() + 4) as i32).to_be_bytes().to_vec();
        packet.extend_from_slice(&startup);
        c.w.write_all(&packet).unwrap();
        c.until_ready();
        if extended {
            for (i, (case, _)) in CASES.iter().enumerate() {
                c.msg(b'P', |b| {
                    cstr(b, &format!("s{i}"));
                    cstr(b, Fenec.text(*case));
                    b.extend_from_slice(&0i16.to_be_bytes());
                });
            }
            c.msg(b'S', |_| {});
            c.send();
            c.until_ready();
        }
        c
    }

    fn msg(&mut self, tag: u8, body: impl FnOnce(&mut Vec<u8>)) {
        self.out.push(tag);
        let at = self.out.len();
        self.out.extend_from_slice(&[0; 4]);
        body(&mut self.out);
        let len = (self.out.len() - at) as i32;
        self.out[at..at + 4].copy_from_slice(&len.to_be_bytes());
    }

    fn send(&mut self) {
        self.w.write_all(&self.out).unwrap();
        self.out.clear();
    }

    /// Reads up to the ReadyForQuery; an ErrorResponse is the bench's end.
    fn until_ready(&mut self) {
        loop {
            let mut head = [0u8; 5];
            self.r.read_exact(&mut head).unwrap();
            let len = i32::from_be_bytes(head[1..5].try_into().unwrap()) as usize - 4;
            self.body.resize(len, 0);
            self.r.read_exact(&mut self.body).unwrap();
            match head[0] {
                b'E' => panic!("{}", String::from_utf8_lossy(&self.body)),
                b'Z' => return,
                _ => {}
            }
        }
    }
}

fn cstr(b: &mut Vec<u8>, s: &str) {
    b.extend_from_slice(s.as_bytes());
    b.push(0);
}

impl Asker for Wire {
    fn ask(&mut self, case: Case, p: &[Param]) {
        if self.extended {
            let i = CASES.iter().position(|(c, _)| *c == case).unwrap();
            self.msg(b'B', |b| {
                cstr(b, "");
                cstr(b, &format!("s{i}"));
                // No format codes: every parameter, and every column, text.
                b.extend_from_slice(&0i16.to_be_bytes());
                b.extend_from_slice(&(p.len() as i16).to_be_bytes());
                for x in p {
                    let v = match x {
                        Param::Int(n) => n.to_string(),
                        Param::Text(s) => s.clone(),
                    };
                    b.extend_from_slice(&(v.len() as i32).to_be_bytes());
                    b.extend_from_slice(v.as_bytes());
                }
                b.extend_from_slice(&0i16.to_be_bytes());
            });
            self.msg(b'E', |b| {
                cstr(b, "");
                b.extend_from_slice(&0i32.to_be_bytes());
            });
            self.msg(b'S', |_| {});
        } else {
            let text = Fenec.inline(case, p);
            self.msg(b'Q', |b| cstr(b, &text));
        }
        self.send();
        self.until_ready();
    }
}

/// `POST /query`, the parameters beside the text, one connection kept alive.
struct Http {
    w: TcpStream,
    r: BufReader<TcpStream>,
    body: Vec<u8>,
}

impl Http {
    fn new(addr: &str) -> Http {
        let w = TcpStream::connect(addr).unwrap();
        w.set_nodelay(true).unwrap();
        let r = BufReader::new(w.try_clone().unwrap());
        Http {
            w,
            r,
            body: Vec::new(),
        }
    }
}

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
        let json = format!(
            "{{\"query\":\"{}\",\"params\":[{}]}}",
            Fenec.text(case),
            params.join(",")
        );
        let head = format!(
            "POST /query HTTP/1.1\r\nHost: bench\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            json.len()
        );
        self.w.write_all(head.as_bytes()).unwrap();
        self.w.write_all(json.as_bytes()).unwrap();
        let mut len = 0usize;
        let mut line = String::new();
        let mut status = String::new();
        loop {
            line.clear();
            self.r.read_line(&mut line).unwrap();
            if status.is_empty() {
                status = line.clone();
            }
            let l = line.trim_end();
            if l.is_empty() {
                break;
            }
            if let Some(v) = l
                .strip_prefix("Content-Length: ")
                .or_else(|| l.strip_prefix("content-length: "))
            {
                len = v.parse().unwrap();
            }
        }
        assert!(
            status.contains(" 200 ") || status.contains(" 201 "),
            "{status}"
        );
        self.body.resize(len, 0);
        self.r.read_exact(&mut self.body).unwrap();
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

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

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
        })
        .unwrap();
    }
    db.checkpoint().unwrap();
}

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_fenec(file: &Path, pg: u16, http: u16) -> Server {
    let bin = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/release/fenec-pg");
    let child = Command::new(&bin)
        .args([
            "--listen",
            &format!("127.0.0.1:{pg}"),
            "--http",
            &format!("127.0.0.1:{http}"),
        ])
        .args(["--file", file.to_str().unwrap(), "--no-checkpoint"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("{}: {e} -- make requests-bench builds it", bin.display()));
    let server = Server(child);
    let until = Instant::now() + Duration::from_secs(30);
    while TcpStream::connect(("127.0.0.1", http)).is_err()
        || TcpStream::connect(("127.0.0.1", pg)).is_err()
    {
        assert!(Instant::now() < until, "fenec-pg did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    server
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

fn round_trip(a: &mut Client) -> f64 {
    let mut lat: Vec<f64> = (0..ONE)
        .map(|_| {
            let t = Instant::now();
            a.simple_query("").unwrap();
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
    let (pg_port, http_port) = (free_port(), free_port());
    let _server = start_fenec(&file, pg_port, http_port);
    let fenec_url = format!("host=127.0.0.1 port={pg_port} user=fenec dbname=fenec");
    let http_addr = format!("127.0.0.1:{http_port}");
    let fenec_wire = format!("127.0.0.1:{pg_port}");
    let pg_url = std::env::var("FENECBENCH_PG").unwrap_or_else(|_| {
        "host=127.0.0.1 port=55432 user=postgres password=fenec dbname=fenecbench".into()
    });
    let pg = pg_setup(&pg_url, &vecs);

    println!("{ROWS} rows x {DIM} dims; one client: round trip p50 / p99 in ms; {CLIENTS} clients: requests a second");
    let mut empty = Client::connect(&fenec_url, NoTls).unwrap();
    print!(
        "empty query's round trip: fenec-pg {:.3} ms",
        round_trip(&mut empty)
    );
    if pg {
        let mut c = Client::connect(&pg_url, NoTls).unwrap();
        print!(
            ", PostgreSQL {:.3} ms (Docker's network)",
            round_trip(&mut c)
        );
    }
    println!("\n");
    println!(
        "{:<28} {:>22} {:>22}",
        "",
        "fenec-pg",
        if pg { "PostgreSQL" } else { "" }
    );
    for (proto, label) in [(0, "extended"), (1, "simple"), (2, "HTTP")] {
        for (case, name) in CASES {
            let mut cells = Vec::new();
            for engine_pg in [false, true] {
                if engine_pg && (!pg || proto == 2) {
                    continue;
                }
                let url = if engine_pg {
                    pg_url.clone()
                } else {
                    fenec_url.clone()
                };
                let (addr, u) = (http_addr.clone(), url.clone());
                let wire = fenec_wire.clone();
                let make = move || -> Box<dyn Asker> {
                    match (proto, engine_pg) {
                        (0, true) => Box::new(Extended::new(&u)),
                        (1, true) => Box::new(Simple {
                            client: pg_client(&u),
                        }),
                        (0, false) => Box::new(Wire::new(&wire, true)),
                        (1, false) => Box::new(Wire::new(&wire, false)),
                        _ => Box::new(Http::new(&addr)),
                    }
                };
                let mut one = make();
                let (p50, p99) = one_at_a_time(one.as_mut(), case, &queries, 42);
                drop(one);
                let rate = at_once(&make, case, &queries);
                cells.push(format!("{p50:.3} / {p99:.3}  {:>7.0}/s", rate));
            }
            println!(
                "{:<28} {:>22} {:>22}",
                format!("{label}: {name}"),
                cells[0],
                cells.get(1).map(String::as_str).unwrap_or("")
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
