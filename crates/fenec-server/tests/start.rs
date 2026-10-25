//! A start that fails: what it leaves behind.
//!
//! A server is restarted by its supervisor as often as its start fails
//! (`Restart=on-failure`), so a failed start must leave the files as it
//! found them. A port that cannot be had is found before anything runs
//! beside the database -- no graph linked, no follower, no mirror -- and
//! the process exits there, with nothing to sync and no checkpoint to
//! rewrite a file of vectors with.

#![cfg(unix)]

use crate::support::{command, documents, ended, start, tmp, Http};
use std::time::Duration;

/// Long enough for a loaded machine; the exit is waited for, not slept
/// through.
const EXIT: Duration = Duration::from_secs(30);

/// A thousand vectors with a graph, written by a server over `args` (a
/// file, or a node whose tenant `acme` they go into) and checkpointed by
/// its SIGTERM.
fn vectors(args: &[&str], node: bool) {
    let server = start(args);
    let prefix = if node { "/t/acme" } else { "" };
    if node {
        let made = server
            .http()
            .with_token("admin")
            .ask("PUT", "/_admin/tenants/acme", "");
        assert_eq!(made.status, 201, "{}", made.body);
    }
    let mut c = server.http();
    let run = |c: &mut Http, q: &str| {
        c.query_at(prefix, q)
            .unwrap_or_else(|e| panic!("{q}: {e:?}"))
    };
    run(&mut c, "create collection v (e vector<8> @hnsw)");
    for block in 0..4 {
        let rows: Vec<String> = (0..250)
            .map(|i| {
                let x = (block * 250 + i) as f32;
                let e: Vec<String> = (0..8).map(|k| (x + k as f32).sin().to_string()).collect();
                format!("{{e: [{}]}}", e.join(", "))
            })
            .collect();
        run(&mut c, &format!("put v [{}]", rows.join(", ")));
    }
    let (code, log) = server.terminate();
    assert_eq!(code, 0, "{log}");
}

/// A file's inode and bytes.
fn as_it_is(path: &std::path::Path) -> (u64, Vec<u8>) {
    use std::os::unix::fs::MetadataExt;
    let ino = std::fs::metadata(path).unwrap().ino();
    (ino, std::fs::read(path).unwrap())
}

/// A port this test holds, so a server asked to listen there cannot.
fn held_port() -> (std::net::TcpListener, String) {
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = held.local_addr().unwrap().to_string();
    (held, addr)
}

/// A server that cannot have its port exits 1 at once, its lock let go,
/// and its file of vectors as it was, to the inode and the byte.
#[test]
fn a_server_that_cannot_listen_leaves_its_file_as_it_was() {
    let path = tmp("start", "unlistened.fenec");
    let p = path.to_str().unwrap();
    vectors(&["--file", p], false);
    let before = as_it_is(&path);

    let (_held, addr) = held_port();
    let (code, log) = ended(command(&["--file", p, "--http", &addr]), EXIT);
    assert_eq!(code, 1, "{log}");
    assert!(log.contains("could not open the HTTP endpoint"), "{log}");
    assert!(!log.contains("checkpoint"), "{log}");
    assert!(as_it_is(&path) == before, "the file changed\n{log}");
    // Its lock is free: a server opens the file and serves it.
    let again = start(&["--file", p]);
    assert!(again.http().run("get v count").contains("1000"));
    drop(again);
    assert_eq!(documents(&path, "v"), 1000);
}

/// The ports come before the history is settled: a file's first start as
/// a primary, which gives it a history of its own and syncs it, writes
/// nothing when its port cannot be had.
#[test]
fn a_first_replicated_start_that_cannot_listen_settles_no_history() {
    let path = tmp("start", "unlistened-primary.fenec");
    let p = path.to_str().unwrap();
    vectors(&["--file", p], false);
    let before = as_it_is(&path);

    let (_held, addr) = held_port();
    let args = ["--file", p, "--replication-token", "r", "--http", &addr];
    let (code, log) = ended(command(&args), EXIT);
    assert_eq!(code, 1, "{log}");
    assert!(log.contains("could not open the HTTP endpoint"), "{log}");
    assert!(as_it_is(&path) == before, "the file changed\n{log}");
}

/// A standby node that cannot have its port opens none of its tenants,
/// which every standby opens to follow them: their files stay as they were.
#[test]
fn a_standby_that_cannot_listen_opens_no_tenant() {
    let dir = tmp("start", "unlistened-node");
    std::fs::create_dir_all(&dir).unwrap();
    let d = dir.to_str().unwrap();
    let node = [
        "--dir",
        d,
        "--admin-token",
        "admin",
        "--replication-token",
        "r",
    ];
    vectors(&node, true);
    let tenant = dir.join("acme.fenec");
    let before = as_it_is(&tenant);

    let (_held, addr) = held_port();
    let mut args = node.to_vec();
    args.extend(["--replica-of", "http://127.0.0.1:1", "--http", &addr]);
    let (code, log) = ended(command(&args), EXIT);
    assert_eq!(code, 2, "{log}");
    assert!(log.contains("could not open the HTTP endpoint"), "{log}");
    assert!(
        as_it_is(&tenant) == before,
        "the tenant's file changed\n{log}"
    );
}

/// A server that stops on a failure once it served writes no checkpoint:
/// `--follow` to a PostgreSQL that refuses it serves first, then its mirror
/// fails, at every try a supervisor makes -- and each rewrote the whole
/// file while it checkpointed.
#[test]
fn a_server_that_stops_on_a_failure_writes_no_checkpoint() {
    let path = tmp("start", "refused-source.fenec");
    let p = path.to_str().unwrap();
    vectors(&["--file", p], false);
    let before = as_it_is(&path);
    for _ in 0..2 {
        let (code, log) = ended(
            command(&[
                "--file",
                p,
                "--http",
                "127.0.0.1:0",
                "--follow",
                "postgres://fenec@127.0.0.1:1/none",
                "--follow-table",
                "t",
            ]),
            EXIT,
        );
        assert_eq!(code, 1, "{log}");
        assert!(log.contains("listening on"), "{log}");
        assert!(log.contains("the mirror of t stopped"), "{log}");
        assert!(!log.contains("checkpoint written"), "{log}");
        assert!(
            as_it_is(&path).0 == before.0,
            "the file was rewritten\n{log}"
        );
    }
    assert_eq!(documents(&path, "v"), 1000);
}

/// An endpoint on an open network with no token is refused with the flags,
/// exit 2, before the file is even made.
#[test]
fn an_open_endpoint_without_a_token_is_refused_before_the_open() {
    let path = tmp("start", "exposed.fenec");
    let p = path.to_str().unwrap();
    let (code, log) = ended(command(&["--file", p, "--http", "0.0.0.0:0"]), EXIT);
    assert_eq!(code, 2, "{log}");
    assert!(log.contains("not a loopback address"), "{log}");
    assert!(!path.exists(), "the file was opened");
}
