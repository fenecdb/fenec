//! A server killed without its shutdown -- a crash -- against a real
//! process: the next one opens its file without linking the vectors the
//! crash left out of the graph, and links them beside its queries.
//!
//! The graph is written only by a checkpoint, and a server checkpoints on
//! its way down. Killed, it leaves every vector it took since in the file's
//! tail; linked at the open, they kept the port closed for as long as they
//! took -- 56.6 s at 100 000 x 768 for a graph never checkpointed.

use fenec_pg::client::{Client, Url};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}
const SIGKILL: i32 = 9;
const SIGTERM: i32 = 15;

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fenecpg-crash-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let _ = std::fs::remove_file(&path);
    path
}

struct Server {
    child: Child,
    port: u16,
    log: Arc<Mutex<String>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Starts the binary on a random port; everything it logs is kept.
fn start(path: &Path, extra: &[&str]) -> Server {
    let mut child = Command::new(env!("CARGO_BIN_EXE_fenec-pg"))
        .args(["--listen", "127.0.0.1:0", "--file"])
        .arg(path)
        .args(extra)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("could not start fenec-pg");
    let mut err = BufReader::new(child.stderr.take().unwrap());
    let log = Arc::new(Mutex::new(String::new()));
    let mut port = None;
    while port.is_none() {
        let mut line = String::new();
        if err.read_line(&mut line).unwrap_or(0) == 0 {
            panic!(
                "fenec-pg ended before it listened:\n{}",
                log.lock().unwrap()
            );
        }
        if let Some(rest) = line.split("localhost:").nth(1) {
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            port = digits.parse().ok();
        }
        log.lock().unwrap().push_str(&line);
    }
    let sink = Arc::clone(&log);
    std::thread::spawn(move || {
        let mut line = String::new();
        while err.read_line(&mut line).unwrap_or(0) > 0 {
            sink.lock().unwrap().push_str(&line);
            line.clear();
        }
    });
    Server {
        child,
        port: port.unwrap(),
        log,
    }
}

impl Server {
    fn client(&self) -> Client {
        Client::connect(&Url {
            user: "fenec".into(),
            password: None,
            host: "127.0.0.1".into(),
            port: self.port,
            database: "fenec".into(),
        })
        .expect("could not connect")
    }

    fn logged(&self, what: &str, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if self.log.lock().unwrap().contains(what) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn signal(mut self, sig: i32) -> String {
        unsafe { kill(self.child.id() as i32, sig) };
        let _ = self.child.wait();
        // The reader thread takes the last lines as the pipe closes.
        std::thread::sleep(Duration::from_millis(100));
        let log = self.log.lock().unwrap().clone();
        log
    }
}

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

fn ids(c: &mut Client, sql: &str) -> Vec<String> {
    let r = c.query(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    r.rows
        .iter()
        .map(|row| row[0].clone().unwrap_or_default())
        .collect()
}

#[test]
fn a_killed_server_links_its_vectors_after_the_open() {
    let path = tmp("killed.fenec");
    let first = start(&path, &["--sync", "always"]);
    let mut c = first.client();
    c.query("create collection t (e vector<8> @hnsw(cosine, m=8))")
        .unwrap();
    for block in 0..20 {
        let rows: Vec<String> = (0..100)
            .map(|i| format!("{{e: {}}}", vector(block * 100 + i)))
            .collect();
        c.query(&format!("put t [{}]", rows.join(", "))).unwrap();
    }
    drop(c);
    // Every write is on disk; the graph never is.
    let log = first.signal(SIGKILL);
    assert!(!log.contains("checkpoint written"), "{log}");

    let second = start(&path, &[]);
    let mut c = second.client();
    let mut found = 0;
    for q in 0..20 {
        let v = vector(10_000 + q);
        let ann = ids(&mut c, &format!("get t select id near e {v} limit 10"));
        let exact = ids(
            &mut c,
            &format!("get t select id near e {v} exact limit 10"),
        );
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
    let log = second.signal(SIGTERM);
    assert!(log.contains("checkpoint written"), "{log}");
    let db = fenec_core::fs::open_serving(&path, true, Box::new(Ok)).expect("reopen");
    assert_eq!(db.unlinked(), 0);
}
