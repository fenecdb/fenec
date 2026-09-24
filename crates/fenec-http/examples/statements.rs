//! What counting a statement by its shape costs (`fenec_http::statements`):
//! `make statements-bench`.
//!
//! Each statement is counted as a server counts it -- its shape written and
//! hashed, its entry found in the thread's shard and added to -- a million
//! times over, by one thread and by eight at once, each with a shard of its
//! own.

use fenec_http::statements;
use std::time::{Duration, Instant};

fn per_call(text: &str, threads: usize, n: usize) -> f64 {
    let t = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                for _ in 0..n {
                    statements::record(None, text, Duration::from_micros(100), false);
                }
            });
        }
    });
    t.elapsed().as_nanos() as f64 / n as f64
}

fn main() {
    let vector: Vec<String> = (0..768)
        .map(|i| format!("{:.6}", (i as f64 * 0.37).sin()))
        .collect();
    let cases = [
        (
            "a read, values in its text",
            "get articles select title where year = 2024 and title = \"rust\" order year desc limit 10"
                .to_string(),
        ),
        (
            "the same, parameters",
            "get articles select title where year = $1 and title = $2 order year desc limit 10"
                .to_string(),
        ),
        (
            "near, 768 components in its text",
            format!("get articles near embed [{}] limit 10", vector.join(", ")),
        ),
    ];
    println!("ns a statement                      1 thread   8 threads   parsed in");
    for (what, text) in &cases {
        per_call(text, 1, 10_000);
        let one = per_call(text, 1, 1_000_000);
        let eight = per_call(text, 8, 1_000_000);
        // What the server does with the text anyway, for scale.
        let t = Instant::now();
        for _ in 0..1_000 {
            let _ = fenec_ql::parse_one(text);
        }
        let parse = t.elapsed().as_nanos() as f64 / 1_000.0;
        println!(
            "{what:<34} {one:>8.0} {eight:>11.0} {parse:>11.0}   ({} bytes)",
            text.len()
        );
    }
}
