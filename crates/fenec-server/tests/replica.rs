//! A primary and a replica as `fenec-server` processes: what a client sees
//! of each over HTTP, a primary killed outright, and the replica promoted.

#[path = "support.rs"]
mod support;

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use support::{count, start, tmp, try_start, Http, Server, SIGKILL, SIGTERM};

const TOKEN: &str = "repl-token";

/// One sample of the scrape, by its exact name.
fn metric(node: &Server, name: &str) -> Option<f64> {
    let a = node.http().ask("GET", "/_metrics", "");
    a.body
        .lines()
        .find_map(|l| l.strip_prefix(name)?.strip_prefix(' ')?.parse().ok())
}

fn post(node: &Server, path: &str) -> (u16, String) {
    let a = node.http().with_token(TOKEN).ask("POST", path, "");
    (a.status, a.body)
}

/// What `/_replication/status` says the node is.
fn role(node: &Server) -> String {
    let a = node
        .http()
        .with_token(TOKEN)
        .ask("GET", "/_replication/status", "");
    assert_eq!(a.status, 200, "{}", a.body);
    support::column(&a.body, "role")[0].clone()
}

fn items(c: &mut Http) -> i64 {
    count(&c.run("get items count"))
}

/// Waits until `c` sees `n` items.
fn sees(c: &mut Http, n: i64) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let got = c.query("get items count").ok().map(|b| count(&b));
        if got == Some(n) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the replica stayed at {got:?} of {n}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_replica_serves_reads_refuses_writes_and_takes_over_when_promoted() {
    let (pf, rf) = (
        tmp("replica", "primary.fenec"),
        tmp("replica", "replica.fenec"),
    );
    let (pf, rf) = (pf.to_str().unwrap(), rf.to_str().unwrap());
    let primary = start(&[
        "--file",
        pf,
        "--replication-token",
        TOKEN,
        "--sync",
        "always",
    ]);
    let mut p = primary.http();
    p.run("create collection items (name text, n int @hash)");
    for i in 0..50 {
        p.run(&format!("put items {{name: \"i{i}\", n: {i}}}"));
    }

    let upstream = format!("http://127.0.0.1:{}", primary.port);
    let replica = start(&[
        "--file",
        rf,
        "--replication-token",
        TOKEN,
        "--replica-of",
        &upstream,
        "--sync",
        "always",
    ]);
    let mut r = replica.http();
    sees(&mut r, 50);
    assert_eq!(
        support::column(&r.run("get items select name where n = 7"), "name"),
        ["i7"]
    );

    // What a client asks to tell the two apart, and what a write gets.
    assert_eq!(role(&primary), "primary");
    assert_eq!(role(&replica), "replica");
    let refused = r.query(r#"put items {name: "no"}"#).unwrap_err();
    assert_eq!(refused.0, 403, "{}", refused.1);

    // Every write the primary acknowledged under `--sync always` reached
    // its disk; killed outright, it takes nothing on with it that the
    // replica was sent and could not keep.
    for i in 50..60 {
        p.run(&format!("put items {{name: \"i{i}\", n: {i}}}"));
    }
    sees(&mut r, 60);
    // What a dashboard watches of the two: the primary feeding one replica,
    // the replica connected and caught up to the same sequence.
    assert_eq!(metric(&primary, "fenec_replicas"), Some(1.0));
    assert_eq!(metric(&replica, "fenec_replica_connected"), Some(1.0));
    let deadline = Instant::now() + Duration::from_secs(20);
    while metric(&replica, "fenec_replica_behind") != Some(0.0) {
        assert!(Instant::now() < deadline, "the replica stayed behind");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        metric(&replica, "fenec_change_sequence"),
        metric(&primary, "fenec_change_sequence")
    );
    let _ = primary.signal(SIGKILL);

    let (status, body) = post(&replica, "/_replication/promote");
    assert_eq!(status, 200, "{body}");
    assert_eq!(role(&replica), "primary");
    r.run(r#"put items {name: "after", n: 60}"#);
    assert_eq!(items(&mut r), 61);

    // Promoted, the file is a primary's: it opens without flags.
    let _ = replica.signal(SIGTERM);
    let reopened = start(&["--file", rf]);
    assert_eq!(items(&mut reopened.http()), 61);
    let _ = reopened.signal(SIGTERM);
}

#[test]
fn a_replicas_file_opens_only_to_follow_or_to_be_promoted() {
    let (pf, rf) = (tmp("replica", "p2.fenec"), tmp("replica", "r2.fenec"));
    let (pf, rf) = (pf.to_str().unwrap(), rf.to_str().unwrap());
    let primary = start(&["--file", pf, "--replication-token", TOKEN, "--sync", "50"]);
    let mut p = primary.http();
    p.run("create collection items (name text)");
    p.run(r#"put items [{name: "a"}, {name: "b"}]"#);
    let upstream = format!("http://127.0.0.1:{}", primary.port);
    let replica = start(&[
        "--file",
        rf,
        "--replication-token",
        TOKEN,
        "--replica-of",
        &upstream,
    ]);
    sees(&mut replica.http(), 2);
    let _ = replica.signal(SIGTERM);

    // Opened as a plain file, it would take writes on the primary's history.
    let err = match try_start(&["--file", rf]) {
        Ok(_) => panic!("a replica's file opened as a plain one"),
        Err(log) => log,
    };
    assert!(err.contains("is a replica's file"), "{err}");

    // `--promote` opens it to writes.
    let promoted = start(&["--file", rf, "--promote"]);
    let mut c = promoted.http();
    c.run(r#"put items {name: "c"}"#);
    assert_eq!(items(&mut c), 3);
    let _ = promoted.signal(SIGTERM);
    let _ = primary.signal(SIGTERM);
}

#[test]
fn replication_refuses_what_it_cannot_do() {
    let path = tmp("replica", "refuse.fenec");
    for (args, says) in [
        (
            vec!["--replication-token", TOKEN, "--sync", "off"],
            "--sync off",
        ),
        (
            vec!["--replica-of", "http://127.0.0.1:1"],
            "--replication-token",
        ),
        (
            vec!["--replication-token", TOKEN, "--replica-of", "https://x:1"],
            "no TLS",
        ),
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_fenec-server"))
            .args(["--http", "127.0.0.1:0", "--file"])
            .arg(&path)
            .args(&args)
            .output()
            .unwrap();
        assert_ne!(out.status.code(), Some(0), "{args:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(says), "{args:?}: {err}");
    }
}

/// A server whose stderr is gone -- the pipe's reader exited, a log
/// collector restarted -- still stops on SIGTERM. `eprintln!` panics on a
/// closed stderr, and the thread that acts on the signal died printing
/// "shutting down", leaving the process up for good.
#[test]
fn a_server_with_no_stderr_still_stops() {
    use std::io::BufRead;
    let path = tmp("replica", "deaf.fenec");
    let mut child = Command::new(env!("CARGO_BIN_EXE_fenec-server"))
        .args(["--http", "127.0.0.1:0", "--file"])
        .arg(&path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("could not start fenec-server");
    let mut err = std::io::BufReader::new(child.stderr.take().unwrap());
    let mut line = String::new();
    while !line.contains("listening on: http://") {
        line.clear();
        assert!(err.read_line(&mut line).unwrap() > 0, "it never listened");
    }
    // Nobody reads its stderr any more.
    drop(err);
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe { kill(child.id() as i32, SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{status}");
            return;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("still running 10 s after SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
