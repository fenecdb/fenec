//! A server killed without its shutdown -- a crash -- against a real
//! process: the next one opens its file without linking the vectors the
//! crash left out of the graph, and links them beside its queries.
//!
//! The graph is written only by a checkpoint, and a server checkpoints on
//! its way down. Killed, it leaves every vector it took since in the file's
//! tail; linked at the open, they kept the port closed for as long as they
//! took -- 56.6 s at 100 000 x 768 for a graph never checkpointed.

use crate::support::{column, start, tmp, Http, SIGKILL, SIGTERM};
use std::time::Duration;

/// A vector for row `i`, spread over the sphere and the same every run.
fn vector(i: u32) -> String {
    let mut x = i.wrapping_mul(0x9E37_79B9) | 1;
    let parts: Vec<String> = (0..8)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            format!("{:.4}", (x % 20_000) as f32 / 10_000.0 - 1.0)
        })
        .collect();
    format!("[{}]", parts.join(", "))
}

fn ids(c: &mut Http, q: &str) -> Vec<String> {
    column(&c.run(q), "id")
}

#[test]
fn a_killed_server_links_its_vectors_after_the_open() {
    let path = tmp("crash", "killed.fenec");
    let file = path.to_str().unwrap();
    let first = start(&["--file", file, "--sync", "always"]);
    let mut c = first.http();
    c.run("create collection t (e vector<8> @hnsw(cosine, m=8))");
    for block in 0..20 {
        let rows: Vec<String> = (0..100)
            .map(|i| format!("{{e: {}}}", vector(block * 100 + i)))
            .collect();
        c.run(&format!("put t [{}]", rows.join(", ")));
    }
    drop(c);
    // Every write is on disk; the graph never is.
    let (_, log) = first.signal(SIGKILL);
    assert!(!log.contains("checkpoint written"), "{log}");

    let second = start(&["--file", file]);
    let mut c = second.http();
    let mut found = 0;
    for q in 0..20 {
        let v = vector(10_000 + q);
        let ann = ids(&mut c, &format!("get t select id near e {v} limit 10"));
        let exact = ids(
            &mut c,
            &format!("get t select id near e {v} exact limit 10"),
        );
        assert_eq!(exact.len(), 10);
        found += ann.iter().filter(|id| exact.contains(id)).count();
    }
    assert!(found >= 190, "recall {found}/200");
    assert!(
        second.logged("2000 vectors linked in", Duration::from_secs(60)),
        "linking never finished:\n{}",
        second.log.lock().unwrap()
    );
    let log = second.log.lock().unwrap().clone();
    assert!(
        log.contains("linking 2000 vectors into the graph beside the queries"),
        "{log}"
    );
    drop(c);

    // Linked, the shutdown's checkpoint writes the whole graph, and the
    // next open has nothing to link.
    let (_, log) = second.signal(SIGTERM);
    assert!(log.contains("checkpoint written"), "{log}");
    let db = fenec_core::fs::open_serving(&path, true, Box::new(Ok)).expect("reopen");
    assert_eq!(db.unlinked(), 0);
}
