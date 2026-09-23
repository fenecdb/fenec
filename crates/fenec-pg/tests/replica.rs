//! A primary and a replica as `fenec-pg` processes: what a PostgreSQL client
//! sees of each, a primary killed outright, and the replica promoted.

use fenec_pg::client::{Client, Url};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}
const SIGKILL: i32 = 9;
const SIGTERM: i32 = 15;
const TOKEN: &str = "repl-token";

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fenecpg-replica-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let _ = std::fs::remove_file(&path);
    path
}

struct Node {
    child: Child,
    pg: u16,
    http: u16,
}

/// Starts the binary with its pg and HTTP listeners on random ports, and
/// waits until both are listening.
fn start(path: &Path, extra: &[&str]) -> Node {
    let mut child = Command::new(env!("CARGO_BIN_EXE_fenec-pg"))
        .args(["--listen", "127.0.0.1:0", "--http", "127.0.0.1:0"])
        .arg("--file")
        .arg(path)
        .args(extra)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("could not start fenec-pg");
    let mut err = BufReader::new(child.stderr.take().unwrap());
    let (mut pg, mut http) = (None, None);
    let port = |line: &str, after: &str| -> Option<u16> {
        let rest = line.split(after).nth(1)?;
        rest.chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .ok()
    };
    let mut log = String::new();
    while pg.is_none() || http.is_none() {
        let mut line = String::new();
        if err.read_line(&mut line).unwrap_or(0) == 0 {
            panic!("fenec-pg ended before it listened:\n{log}");
        }
        log.push_str(&line);
        if line.contains("postgres://localhost:") {
            pg = port(&line, "localhost:");
        }
        // Its own listener's line: a replica also names its primary's.
        if line.contains("listening on: http://127.0.0.1:") {
            http = port(&line, "127.0.0.1:");
        }
    }
    // Keep reading stderr so the process never blocks on a full pipe.
    std::thread::spawn(move || {
        let mut sink = String::new();
        let _ = err.read_to_string(&mut sink);
    });
    Node {
        child,
        pg: pg.unwrap(),
        http: http.unwrap(),
    }
}

impl Node {
    fn client(&self) -> Client {
        Client::connect(&Url {
            user: "fenec".into(),
            password: None,
            host: "127.0.0.1".into(),
            port: self.pg,
            database: "fenec".into(),
        })
        .expect("could not connect")
    }

    fn signal(mut self, sig: i32) {
        unsafe { kill(self.child.id() as i32, sig) };
        let _ = self.child.wait();
    }

    /// One sample of the scrape, by its exact name.
    fn metric(&self, name: &str) -> Option<f64> {
        let mut s = TcpStream::connect(("127.0.0.1", self.http)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        write!(
            s,
            "GET /_metrics HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out.lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(' ')?.parse().ok())
    }

    fn post(&self, path: &str) -> (u16, String) {
        let mut s = TcpStream::connect(("127.0.0.1", self.http)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        write!(
            s,
            "POST {path} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {TOKEN}\r\n\
             Content-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        let body = out
            .split_once("\r\n\r\n")
            .map_or("", |(_, b)| b)
            .to_string();
        (out[9..12].parse().unwrap(), body)
    }
}

/// A node the test did not stop -- an assertion failed first -- is killed
/// rather than left running: a replica whose primary is gone retries it
/// forever.
impl Drop for Node {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            unsafe { kill(self.child.id() as i32, SIGKILL) };
            let _ = self.child.wait();
        }
    }
}

fn count(c: &mut Client) -> usize {
    let r = c.query("get items count").unwrap();
    r.rows[0][0].as_deref().unwrap().parse().unwrap()
}

fn one(c: &mut Client, sql: &str) -> String {
    c.query(sql).unwrap().rows[0][0].clone().unwrap_or_default()
}

/// Waits until `c` sees `n` items.
fn sees(c: &mut Client, n: usize) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let got = c.query("get items count").ok().map(|r| {
            r.rows[0][0]
                .as_deref()
                .unwrap_or("0")
                .parse::<usize>()
                .unwrap_or(0)
        });
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
    let (pf, rf) = (tmp("primary.fenec"), tmp("replica.fenec"));
    let primary = start(&pf, &["--replication-token", TOKEN, "--sync", "always"]);
    let mut p = primary.client();
    p.query("create collection items (name text, n int @hash)")
        .unwrap();
    for i in 0..50 {
        p.query(&format!("put items {{name: \"i{i}\", n: {i}}}"))
            .unwrap();
    }

    let upstream = format!("http://127.0.0.1:{}", primary.http);
    let replica = start(
        &rf,
        &[
            "--replication-token",
            TOKEN,
            "--replica-of",
            &upstream,
            "--sync",
            "always",
        ],
    );
    let mut r = replica.client();
    sees(&mut r, 50);
    assert_eq!(one(&mut r, "get items select name where n = 7"), "i7");

    // What a driver asks to tell the two apart, and what a write gets.
    assert_eq!(one(&mut p, "SHOW transaction_read_only"), "off");
    assert_eq!(one(&mut r, "SHOW transaction_read_only"), "on");
    assert_eq!(one(&mut r, "SELECT pg_catalog.pg_is_in_recovery()"), "t");
    let refused = r.query("put items {name: \"no\"}").unwrap_err().to_string();
    assert!(refused.contains("25006"), "{refused}");

    // Every write the primary acknowledged under `--sync always` reached
    // its disk; killed outright, it takes nothing on with it that the
    // replica was sent and could not keep.
    for i in 50..60 {
        p.query(&format!("put items {{name: \"i{i}\", n: {i}}}"))
            .unwrap();
    }
    sees(&mut r, 60);
    // What a dashboard watches of the two: the primary feeding one replica,
    // the replica connected and caught up to the same sequence.
    assert_eq!(primary.metric("fenec_replicas"), Some(1.0));
    assert_eq!(replica.metric("fenec_replica_connected"), Some(1.0));
    let deadline = Instant::now() + Duration::from_secs(20);
    while replica.metric("fenec_replica_behind") != Some(0.0) {
        assert!(Instant::now() < deadline, "the replica stayed behind");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        replica.metric("fenec_change_sequence"),
        primary.metric("fenec_change_sequence")
    );
    primary.signal(SIGKILL);

    let (status, body) = replica.post("/_replication/promote");
    assert_eq!(status, 200, "{body}");
    assert_eq!(one(&mut r, "SHOW transaction_read_only"), "off");
    r.query("put items {name: \"after\", n: 60}").unwrap();
    assert_eq!(count(&mut r), 61);

    // Promoted, the file is a primary's: it opens without flags.
    replica.signal(SIGTERM);
    let reopened = start(&rf, &[]);
    assert_eq!(count(&mut reopened.client()), 61);
    reopened.signal(SIGTERM);
}

#[test]
fn a_replicas_file_opens_only_to_follow_or_to_be_promoted() {
    let (pf, rf) = (tmp("p2.fenec"), tmp("r2.fenec"));
    let primary = start(&pf, &["--replication-token", TOKEN, "--sync", "50"]);
    let mut p = primary.client();
    p.query("create collection items (name text)").unwrap();
    p.query("put items [{name: \"a\"}, {name: \"b\"}]").unwrap();
    let upstream = format!("http://127.0.0.1:{}", primary.http);
    let replica = start(
        &rf,
        &["--replication-token", TOKEN, "--replica-of", &upstream],
    );
    sees(&mut replica.client(), 2);
    replica.signal(SIGTERM);

    // Opened as a plain file, it would take writes on the primary's history.
    let out = Command::new(env!("CARGO_BIN_EXE_fenec-pg"))
        .args(["--listen", "127.0.0.1:0", "--file"])
        .arg(&rf)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("is a replica's file"), "{err}");

    // `--promote` opens it to writes.
    let promoted = start(&rf, &["--promote"]);
    let mut c = promoted.client();
    c.query("put items {name: \"c\"}").unwrap();
    assert_eq!(count(&mut c), 3);
    promoted.signal(SIGTERM);
    primary.signal(SIGTERM);
}

#[test]
fn replication_refuses_what_it_cannot_do() {
    let path = tmp("refuse.fenec");
    for (args, says) in [
        (
            vec![
                "--replication-token",
                TOKEN,
                "--sync",
                "off",
                "--http",
                "127.0.0.1:0",
            ],
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
        let out = Command::new(env!("CARGO_BIN_EXE_fenec-pg"))
            .args(["--listen", "127.0.0.1:0", "--file"])
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
    let path = tmp("deaf.fenec");
    let mut child = Command::new(env!("CARGO_BIN_EXE_fenec-pg"))
        .args(["--listen", "127.0.0.1:0", "--file"])
        .arg(&path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("could not start fenec-pg");
    let mut err = BufReader::new(child.stderr.take().unwrap());
    let mut line = String::new();
    while !line.contains("postgres://localhost:") {
        line.clear();
        assert!(err.read_line(&mut line).unwrap() > 0, "it never listened");
    }
    // Nobody reads its stderr any more.
    drop(err);
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
