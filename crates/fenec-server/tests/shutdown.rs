//! The shutdown path and the syncer -- against a real process.
//!
//! These behaviours cannot be exercised *inside* the process: shutdown ends
//! in `process::exit` and the shutdown flag belongs to the whole process.
//! So the test runs the `fenec-server` binary, sends SIGTERM and inspects
//! the file left behind -- exactly what `docker stop` does.

use crate::support::{documents, json_string, start, tmp, SIGKILL};
use std::time::{Duration, Instant};

/// `--sync off`: nothing reaches the disk except on shutdown. So every row
/// found in the file went through the shutdown path -- a write lost between
/// `sync` and `exit` shows up as missing here.
#[test]
fn sigterm_flushes_pending_writes() {
    let path = tmp("shutdown", "drain.fenec");
    let server = start(&["--file", path.to_str().unwrap(), "--sync", "off"]);

    let mut c = server.http();
    c.run("create collection t (name text)");
    c.run(r#"put t {name: "one"}"#);
    c.run(r#"put t {name: "two"}"#);

    let before = std::fs::metadata(&path).unwrap().len();
    let (code, log) = server.terminate();

    assert_eq!(code, 0, "expected a clean exit\n{log}");
    assert!(
        log.contains("shutting down"),
        "the shutdown hook did not run\n{log}"
    );
    let after = std::fs::metadata(&path).unwrap().len();
    assert!(
        after > before,
        "writes did not reach the disk on shutdown ({before} -> {after} bytes)"
    );
    assert_eq!(documents(&path, "t"), 2, "a write was lost");
}

/// `--sync <ms>`: the syncer pushes what was written to disk within its
/// interval, with no shutdown and no further write.
#[test]
fn interval_sync_flushes_in_background() {
    let path = tmp("shutdown", "interval.fenec");
    let server = start(&["--file", path.to_str().unwrap(), "--sync", "50"]);
    let mut c = server.http();
    c.run("create collection t (name text)");
    c.run(r#"put t {name: "delayed"}"#);

    let deadline = Instant::now() + Duration::from_secs(5);
    while documents(&path, "t") == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        documents(&path, "t"),
        1,
        "the periodic syncer did not push the write to disk"
    );
}

/// Killed, a server holds every batch it answered, whole, and nothing of
/// one cut short: a batch's writes reach the file as one record, after
/// its last statement ran.
#[test]
fn a_killed_server_holds_every_batch_it_answered_whole() {
    let path = tmp("shutdown", "killed-batch.fenec");
    let server = start(&["--file", path.to_str().unwrap(), "--sync", "always"]);
    let mut c = server.http();
    c.run("create collection t (name text)");
    c.run("create collection u (n int)");
    let line = |q: &str| format!("{{\"query\": {}}}", json_string(q));
    let batch = [
        line(r#"put t {name: "a"}"#),
        line("put u {n: 1}"),
        line(r#"put t {name: "b"}"#),
    ]
    .join("\n");
    let a = c.ask("POST", "/batch", &batch);
    assert_eq!(a.status, 200, "{}", a.body);
    // One that fails at its last statement leaves nothing.
    let failed = [
        line(r#"put t {name: "never"}"#),
        line("put u {n: 2}"),
        line("put nowhere {n: 3}"),
    ]
    .join("\n");
    let a = c.ask("POST", "/batch", &failed);
    assert_ne!(a.status, 200, "{}", a.body);

    let (code, _) = server.signal(SIGKILL);
    assert_ne!(code, 0, "the server was not killed");
    assert_eq!(documents(&path, "t"), 2);
    assert_eq!(documents(&path, "u"), 1);
}

/// Checkpoint on shutdown: the HNSW graph lands in the file. With
/// `--no-checkpoint` it does not -- the difference shows in the file size,
/// not only in the log.
#[test]
fn checkpoint_on_exit_writes_the_graph() {
    let rows: Vec<String> = (0..64)
        .map(|i| {
            let a = (i % 8) as f32 / 8.0;
            let b = (i % 5) as f32 / 5.0;
            format!("{{name: \"d{i}\", e: [{a}, {b}, 0.5, 0.25]}}")
        })
        .collect();
    let put = format!("put t [{}]", rows.join(", "));

    let mut size = [0u64; 2];
    for (i, extra) in [Vec::new(), vec!["--no-checkpoint"]].iter().enumerate() {
        let path = tmp(
            "shutdown",
            if i == 0 {
                "cp-on.fenec"
            } else {
                "cp-off.fenec"
            },
        );
        let mut args = vec!["--file", path.to_str().unwrap()];
        args.extend(extra);
        let server = start(&args);
        let mut c = server.http();
        c.run("create collection t (name text, e vector<4> @hnsw(cosine))");
        c.run(&put);

        let (code, log) = server.terminate();
        assert_eq!(code, 0, "expected a clean exit\n{log}");
        assert_eq!(
            log.contains("checkpoint written"),
            i == 0,
            "the checkpoint behaved unexpectedly (--no-checkpoint: {})\n{log}",
            i == 1
        );
        // The data is durable either way; only the graph differs.
        assert_eq!(documents(&path, "t"), 64, "a write was lost");
        size[i] = std::fs::metadata(&path).unwrap().len();
    }

    assert!(
        size[0] > size[1],
        "the graph was not written to the file: {} bytes with a checkpoint, {} without",
        size[0],
        size[1]
    );
}

/// `--ping` asks `GET /_health` at the `--http` address: 0 while a server
/// answers there, 1 once none does -- whatever the environment it inherits
/// from the server's container would configure, which a probe never names
/// the rest of on its command line.
#[test]
fn ping_asks_the_http_listener() {
    let server = start(&[]);
    let addr = format!("127.0.0.1:{}", server.port);
    let ping = |env: &[(&str, &str)]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_fenec-server"))
            .args(["--ping", "--http", &addr])
            .envs(env.iter().copied())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .code()
    };
    let secret = [("FENEC_JWT_SECRET", "a secret of at least thirty-two bytes")];
    assert_eq!(ping(&[]), Some(0));
    assert_eq!(
        ping(&secret),
        Some(0),
        "a JWT secret with no --policy failed the probe"
    );
    let _ = server.terminate();
    assert_eq!(ping(&[]), Some(1));
    assert_eq!(ping(&secret), Some(1));
}

/// Claims held until a job comes (`Fenec-Wait`) are answered at SIGTERM,
/// with the nothing they found, before the process ends: a worker learns
/// of the shutdown from an answer, not from a broken connection. A file's
/// server and a node of tenants alike.
#[test]
fn sigterm_answers_held_claims_at_once() {
    for dir in [false, true] {
        let path = tmp("shutdown", if dir { "held-dir" } else { "held.fenec" });
        let flag = if dir { "--dir" } else { "--file" };
        let server = start(&[
            flag,
            path.to_str().unwrap(),
            "--sync",
            "off",
            "--admin-token",
            "adm",
        ]);
        let prefix = if dir { "/t/acme" } else { "" };
        let mut c = server.http().with_token("adm");
        if dir {
            let a = c.ask("PUT", "/_admin/tenants/acme", "");
            assert!(a.status < 300, "{}", a.body);
        }
        c.query_at(
            prefix,
            "create collection jobs (run_at timestamp @sorted, owner text)",
        )
        .unwrap();
        let port = server.port;
        let target = format!("{prefix}/query");
        let held: Vec<_> = (0..10)
            .map(|w| {
                let target = target.clone();
                std::thread::spawn(move || {
                    let claim = format!(
                        "set jobs {{owner: \\\"w{w}\\\", run_at: now() + 60000}} \
                         where run_at <= now() order run_at limit 1 returning id"
                    );
                    let body = format!("{{\"query\": \"{claim}\"}}");
                    let mut c = crate::support::Http::open(port);
                    let a = c
                        .try_ask("POST", &target, &body, &[("Fenec-Wait", "30000")])
                        .expect("answered before the process ended");
                    (a.status, a.body, Instant::now())
                })
            })
            .collect();
        // Held: the metric says so.
        let t = Instant::now();
        loop {
            let m = c.ask("GET", "/_metrics", "").body;
            if m.contains("fenec_held_requests 10") {
                break;
            }
            assert!(t.elapsed() < Duration::from_secs(10), "{m}");
            std::thread::sleep(Duration::from_millis(5));
        }
        let signalled = Instant::now();
        let (code, log) = server.terminate();
        assert_eq!(code, 0, "{log}");
        for h in held {
            let (status, body, at) = h.join().unwrap();
            assert_eq!((status, body.as_str()), (200, "[]"));
            let after = at - signalled;
            assert!(
                after < Duration::from_secs(1),
                "answered {after:?} after SIGTERM"
            );
        }
    }
}
