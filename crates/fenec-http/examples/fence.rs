//! What a node's lease costs a write: `make fence-bench`
//! (`cargo run --release -p fenec-http --example fence -- [ROUNDS]`).
//!
//!   * A lone `put` in process into a database with no fence, and into one
//!     whose fence is a node's lease, as a tenant on a `--lease` node has it
//!     (`Database::set_fence`, asked as each block lands), in turns: ns a
//!     put, the median of the rounds of 1 000.
//!   * `Lease::allows` alone, which the fence is, and which a tenant's
//!     request asks before it runs and again as it is answered: ns a call,
//!     the median of the rounds of 100 000.
//!
//! The two sides of each pair take turns, which goes first changing every
//! round, so a machine slowing down slows both alike. Between two builds --
//! the lease measured on one clock, then another -- compare the fenced put
//! and `allows` run after run, in ABBA turns.

use fenec_http::lease::Lease;
use std::sync::Arc;
use std::time::Instant;

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

fn main() {
    let rounds: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(41);
    let lease = Arc::new(Lease::new());
    assert!(lease
        .grant(3_600_000, "e1", Some(vec!["acme".into()]))
        .is_ok());
    let put = fenec_ql::parse_one("put docs {title: \"a row of some length\", n: 7}").unwrap();
    let make = |fenced: bool| {
        let mut db = fenec_core::prelude::Database::new();
        db.execute(&fenec_ql::parse_one("create collection docs (title text, n int)").unwrap())
            .unwrap();
        if fenced {
            let lease = Arc::clone(&lease);
            db.set_fence(Some(Arc::new(move || lease.allows("acme"))));
        }
        db
    };
    let mut dbs = [make(false), make(true)];
    let mut puts = [Vec::new(), Vec::new()];
    let mut allows = Vec::new();
    for round in 0..rounds {
        for k in 0..2 {
            let side = (k + round) % 2;
            let t = Instant::now();
            for _ in 0..1000 {
                dbs[side].execute(&put).unwrap();
            }
            puts[side].push(t.elapsed().as_nanos() as f64 / 1000.0);
        }
        let t = Instant::now();
        for _ in 0..100_000 {
            assert!(std::hint::black_box(&lease).allows("acme").is_ok());
        }
        allows.push(t.elapsed().as_nanos() as f64 / 100_000.0);
    }
    let [free, fenced] = &mut puts;
    let (f, l) = (median(free), median(fenced));
    println!("a lone put in process, the median of {rounds} rounds of 1 000:");
    println!("  no fence            {f:>7.0} ns");
    println!(
        "  the lease's fence   {l:>7.0} ns   ({:+.0} ns, {:+.1}%)",
        l - f,
        (l / f - 1.0) * 100.0
    );
    println!(
        "Lease::allows alone  {:>7.1} ns, the median of {rounds} rounds of 100 000",
        median(&mut allows)
    );
}
