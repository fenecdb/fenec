//! A paged read whose page size and place change from request to request,
//! its `limit` and `offset` written into the text against sent as
//! parameters: `make pages-bench`.
//!
//! fenec-server is started over a file of `ROWS` rows with an ordered
//! field, and asked for `get docs select id, score, title order score limit
//! L offset O` -- L one of `SIZES`, O a page of that size drawn at random,
//! as a list a user jumps through is asked -- by one client at a time (the
//! round trip's p50) and by `CLIENTS` at once (requests a second). Written
//! into the text, every size and page is a text of its own: `POST /query`
//! parses it unless its parse cache (16 shards of 64, a shard emptied when
//! full) still holds it. As parameters it is one text, parsed once.
//!
//! A server built with `--features timing` also answers `GET /_timing`,
//! whose `parsed` counts the requests that parsed their statement: run
//! against one (`FENECBENCH_SERVER=target/timing/release/fenec-server`),
//! the bench prints the parse cache's hit rate and the mean time of the
//! request's `cache` phase, the lookup or the parse.

#[path = "../http.rs"]
mod http;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use fenec_core::prelude::*;

const ROWS: usize = 5_000;
const SIZES: [usize; 4] = [10, 20, 25, 50];
const ONE: usize = 5_000;
const CLIENTS: usize = 8;
const RUN: Duration = Duration::from_secs(3);

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A page: its size, and where it starts.
    fn page(&mut self) -> (usize, usize) {
        let size = SIZES[(self.next() % SIZES.len() as u64) as usize];
        let pages = ROWS / size;
        (size, (self.next() % pages as u64) as usize * size)
    }
}

const PARAMETERS: &str = "get docs select id, score, title order score limit $1 offset $2";

/// One page asked, its counts written in (`literal`) or as parameters.
fn ask(c: &mut http::Http, literal: bool, (limit, offset): (usize, usize)) {
    let rows = if literal {
        let text =
            format!("get docs select id, score, title order score limit {limit} offset {offset}");
        http::ids(c.query(&text, ""))
    } else {
        http::ids(c.query(PARAMETERS, &format!("{limit},{offset}")))
    };
    assert_eq!(rows.len(), limit);
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[((v.len() - 1) as f64 * p) as usize]
}

/// p50 and p99 in ms of `ONE` pages asked one at a time.
fn one_at_a_time(addr: &str, literal: bool) -> (f64, f64) {
    let mut c = http::Http::connect(addr);
    let mut rng = Rng(42);
    for _ in 0..500 {
        ask(&mut c, literal, rng.page());
    }
    let mut lat = Vec::with_capacity(ONE);
    for _ in 0..ONE {
        let page = rng.page();
        let t = Instant::now();
        ask(&mut c, literal, page);
        lat.push(t.elapsed().as_secs_f64() * 1e3);
    }
    (pct(&mut lat, 0.5), pct(&mut lat, 0.99))
}

/// Pages a second from `CLIENTS` clients asking at once for `RUN`.
fn at_once(addr: &str, literal: bool) -> f64 {
    let done = AtomicU64::new(0);
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        for c in 0..CLIENTS as u64 {
            let (done, stop) = (&done, &stop);
            s.spawn(move || {
                let mut conn = http::Http::connect(addr);
                let mut rng = Rng(1000 + c);
                let mut n = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    ask(&mut conn, literal, rng.page());
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

/// `GET /_timing`'s `key`, where the server was built with it.
fn timing(c: &mut http::Http, key: &str) -> Option<f64> {
    let (status, body) = c.request("GET", "/_timing", "text/plain", b"");
    if status != 200 {
        return None;
    }
    let body = String::from_utf8_lossy(body);
    let at = body.find(&format!("\"{key}\":"))? + key.len() + 3;
    body[at..].split([',', '}']).next()?.trim().parse().ok()
}

fn fenec_file(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let mut db = fenec_core::fs::open(path).unwrap();
    let run = |db: &mut Database, sql: &str, params: &[Value]| {
        db.execute_with(&fenec_ql::parse_one(sql).unwrap(), params)
            .unwrap();
    };
    run(
        &mut db,
        "create collection docs (score int @sorted, title text)",
        &[],
    );
    let mut rng = Rng(7);
    db.begin().unwrap();
    for i in 0..ROWS {
        run(
            &mut db,
            "put docs {score: $1, title: $2}",
            &[
                Value::Int((rng.next() % 1_000_000) as i64),
                Value::Text(format!("row {i}")),
            ],
        );
    }
    db.commit().unwrap();
    db.checkpoint().unwrap();
}

fn main() {
    let dir = std::env::temp_dir().join(format!("fenecbench-pages-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("pages.fenec");
    fenec_file(&file);
    let port = http::free_port();
    let _server = http::start_fenec(&file, port, "pages-bench", &[]);
    let addr = format!("127.0.0.1:{port}");
    let mut admin = http::Http::connect(&addr);
    let texts: usize = SIZES.iter().map(|s| ROWS / s).sum();
    println!(
        "{ROWS} rows, pages of {SIZES:?} at random: {texts} texts written in, 1 as parameters; \
         one client: round trip p50 / p99 in ms; {CLIENTS} clients: pages a second"
    );
    let timed = timing(&mut admin, "requests").is_some();
    // A server from before `limit $n` (`FENECBENCH_SERVER`, another
    // commit's build) is asked the texts written in alone.
    let body = format!(
        "{{\"query\":{},\"params\":[1,0]}}",
        http::json_string(PARAMETERS)
    );
    let takes = admin
        .request("POST", "/query", "application/json", body.as_bytes())
        .0
        == 200;
    let ways: &[(bool, &str)] = match takes {
        true => &[(true, "written in"), (false, "parameters")],
        false => &[(true, "written in")],
    };
    // In turns, twice, so that neither way has the machine to itself.
    for round in 0..2 {
        for &(literal, name) in ways {
            admin.request("DELETE", "/_timing", "text/plain", b"");
            let (p50, p99) = one_at_a_time(&addr, literal);
            let rate = at_once(&addr, literal);
            let mut line =
                format!("round {round} {name:<11} {p50:.3} / {p99:.3} ms  {rate:>8.0}/s");
            if timed {
                let requests = timing(&mut admin, "requests").unwrap_or(0.0);
                let parsed = timing(&mut admin, "parsed").unwrap_or(0.0);
                let cache = timing(&mut admin, "cache").unwrap_or(0.0);
                line.push_str(&format!(
                    "  parse cache hits {:.1}% of {requests:.0}, its phase {cache:.3} us",
                    100.0 * (1.0 - parsed / requests.max(1.0))
                ));
            }
            println!("{line}");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
