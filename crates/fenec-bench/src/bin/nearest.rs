//! Claiming the nearest ready order: `make nearest-bench`.
//!
//! A courier's claim takes the ready order nearest them within 3 km under a
//! lease. The dispatch example (`examples/dispatch`) wrote it in three
//! steps while FenecQL could not say it in one: a read of the six nearest
//! ready orders, then a `/batch` of a conditional `set` a candidate, each
//! asking that none before it took one, and a read back of what was taken.
//! Here that is held against the claim as one statement, two ways:
//!
//!   * `order`: `set ... where run_at <= now() and distance(pickup, $p) <=
//!     3000 order distance(pickup, $p) limit 1 returning id`, the point
//!     index walked nearest first under the write lock;
//!   * `inner`: `set ... where id in (get orders select id where ... near
//!     pickup $p limit 1) returning id`, the inner `get` run first under the
//!     same lock;
//!   * `three`: the example's read and `/batch`, two round trips.
//!
//! Over HTTP against fenec-server, 1, 16 and 50 claimers at once, each
//! from a random point of a 20 km city until it has made its claims: a
//! claim's p50 and p99, claims a second, and for `three` the batches that
//! took nothing because another courier's took their candidates first.
//! `ORDERS=50000` and `CLAIMS=100` (each claimer's) shrink it; the default is
//! 300 000 orders, of which the rounds lease about 80 000 between them.

#[path = "../http.rs"]
mod http;

use fenec_core::prelude::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// The city's centre, and its half width in degrees: about 10 km.
const CENTRE: (f64, f64) = (29.05, 41.05);
const HALF: f64 = 0.09;

const ORDER: &str = "set orders {status: \"assigned\", courier: $1, claim: $2, run_at: now() + 45000, \
                     attempts: attempts + 1} where run_at <= now() and distance(pickup, $3) <= 3000 \
                     order distance(pickup, $3) limit 1 returning id";
const INNER: &str = "set orders {status: \"assigned\", courier: $1, claim: $2, run_at: now() + 45000, \
                     attempts: attempts + 1} where id in (get orders select id where run_at <= now() \
                     and distance(pickup, $3) <= 3000 near pickup $3 limit 1) returning id";
/// The example's: the six nearest ready orders ...
const NEAREST: &str =
    "get orders select id where run_at <= now() and distance(pickup, $1) <= 3000 \
                       near pickup $1 limit 6";
/// ... each taken if still ready and none before it in the batch took one ...
const TAKE_IF_FIRST: &str = "set orders {status: \"assigned\", courier: $1, claim: $2, \
                             run_at: now() + 45000, attempts: attempts + 1} where id = $3 and \
                             run_at <= now() and not ($2 in (get orders select claim where claim = $2))";
/// ... and what was taken, read back.
const TAKEN: &str = "get orders select id where claim = $1";

fn env(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(default)
}

struct Rng(u64);
impl Rng {
    fn unit(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
    fn point(&mut self) -> String {
        let lon = CENTRE.0 + (self.unit() * 2.0 - 1.0) * HALF * 1.33;
        let lat = CENTRE.1 + (self.unit() * 2.0 - 1.0) * HALF;
        format!("[{lon:.6}, {lat:.6}]")
    }
}

/// `n` ready orders over the city, indexed as the dispatch example's are.
fn seed(db: &mut Database, n: usize) {
    let run = |db: &mut Database, sql: &str| {
        let stmt = fenec_ql::parse_one_for(db, sql).unwrap();
        db.execute(&stmt).unwrap();
    };
    run(
        db,
        "create collection orders (pickup geo @geo, run_at timestamp @sorted, status text @hash, \
         courier text @hash, claim text @hash, attempts int)",
    );
    let mut r = Rng(0x0dde55);
    for b in 0..n.div_ceil(1000) {
        let docs: Vec<String> = (b * 1000..((b + 1) * 1000).min(n))
            .map(|_| {
                format!(
                    "{{pickup: {}, run_at: 1, status: \"ready\", attempts: 0}}",
                    r.point()
                )
            })
            .collect();
        run(db, &format!("put orders [{}]", docs.join(", ")));
    }
}

fn percentiles(mut us: Vec<f64>) -> String {
    us.sort_by(|a, b| a.total_cmp(b));
    let at = |p: f64| us[((us.len() as f64 * p) as usize).min(us.len() - 1)];
    format!("p50 {:.3} ms, p99 {:.3}", at(0.5) / 1e3, at(0.99) / 1e3)
}

/// `claimers` couriers claiming `claims` orders each, the claim `how`.
fn round(addr: &str, how: &'static str, claimers: usize, claims: usize, seed: u64) -> String {
    let empty = Arc::new(AtomicUsize::new(0));
    let at = Instant::now();
    let hs: Vec<_> = (0..claimers)
        .map(|t| {
            let (addr, empty) = (addr.to_string(), empty.clone());
            std::thread::spawn(move || {
                let mut c = http::Http::connect(&addr);
                let mut r = Rng(seed ^ (t as u64 + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15));
                let mut us = Vec::with_capacity(claims);
                for k in 0..claims {
                    let p = r.point();
                    let who = format!("\"c{t}\"");
                    let claim = format!("\"c{t}:{k}:{seed}\"");
                    let began = Instant::now();
                    let took = match how {
                        "order" | "inner" => {
                            let sql = if how == "order" { ORDER } else { INNER };
                            !http::ids(c.query(sql, &format!("{who}, {claim}, {p}"))).is_empty()
                        }
                        _ => {
                            let ids = http::ids(c.query(NEAREST, &p));
                            let mut lines: Vec<String> = ids
                                .iter()
                                .map(|id| {
                                    format!(
                                        "{{\"query\":{},\"params\":[{who},{claim},{id}]}}",
                                        http::json_string(TAKE_IF_FIRST)
                                    )
                                })
                                .collect();
                            lines.push(format!(
                                "{{\"query\":{},\"params\":[{claim}]}}",
                                http::json_string(TAKEN)
                            ));
                            let body = c.post(
                                "/batch",
                                "application/x-ndjson",
                                lines.join("\n").as_bytes(),
                            );
                            String::from_utf8_lossy(body).contains("{\"id\":")
                        }
                    };
                    us.push(began.elapsed().as_secs_f64() * 1e6);
                    if !took {
                        empty.fetch_add(1, Ordering::Relaxed);
                    }
                }
                us
            })
        })
        .collect();
    let mut us = Vec::new();
    for h in hs {
        us.extend(h.join().unwrap());
    }
    let s = at.elapsed().as_secs_f64();
    format!(
        "{how:5} {claimers:>2} claimers: {}, {:.0} claims/s, {} took none",
        percentiles(us.clone()),
        us.len() as f64 / s,
        empty.load(Ordering::Relaxed)
    )
}

fn main() {
    let (orders, claims) = (env("ORDERS", 300_000), env("CLAIMS", 200));
    let dir = std::env::temp_dir().join(format!("fenecbench-nearest-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("orders.fenec");
    {
        let mut db = fenec_core::fs::open(&file).unwrap();
        seed(&mut db, orders);
        db.checkpoint().unwrap();
    }
    let port = http::free_port();
    let server = http::start_fenec(&file, port, "nearest-bench", &[]);
    let addr = format!("127.0.0.1:{port}");
    println!("{orders} ready orders over a 20 km city, each claimer {claims} claims within 3 km:");
    // Each round in turns, a fresh seed of points: the orders claimed
    // before stay leased, and the rounds take their nearest from what is
    // left, as a day's couriers do.
    let mut seed = 1;
    for claimers in [1, 16, 50] {
        for how in ["order", "inner", "three", "three", "inner", "order"] {
            seed += 1;
            println!("  {}", round(&addr, how, claimers, claims, seed));
        }
    }
    server.terminate();
    let _ = std::fs::remove_dir_all(&dir);
}
