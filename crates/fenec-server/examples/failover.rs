//! What a failover loses: `cargo run --release -p fenec-server --example failover -- [TRIALS]`
//!
//! Needs `target/release/fenec-server` (`make replica-bench` builds it). Per
//! sync policy and trial: a primary and a replica as processes, one client
//! writing to the primary over HTTP as fast as it answers, the primary
//! killed with SIGKILL mid-stream, the replica promoted. Counted: the
//! writes the client was told succeeded that the promoted replica does not
//! hold.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

const TOKEN: &str = "t";

struct Node {
    child: Child,
    http: u16,
}

fn binary() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    // target/release/examples/failover -> target/release/fenec-server
    exe.parent().unwrap().parent().unwrap().join("fenec-server")
}

fn start(path: &Path, extra: &[&str]) -> Node {
    let mut child = Command::new(binary())
        .args(["--http", "127.0.0.1:0", "--file"])
        .arg(path)
        .args(extra)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("fenec-server: run `cargo build --release -p fenec-server` first");
    let mut err = BufReader::new(child.stderr.take().unwrap());
    let mut http = None;
    while http.is_none() {
        let mut line = String::new();
        if err.read_line(&mut line).unwrap_or(0) == 0 {
            panic!("fenec-server ended before it listened");
        }
        if let Some(rest) = line.split("listening on: http://127.0.0.1:").nth(1) {
            http = rest
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .ok();
        }
    }
    std::thread::spawn(move || {
        let mut sink = String::new();
        let _ = err.read_to_string(&mut sink);
    });
    Node {
        child,
        http: http.unwrap(),
    }
}

/// A keep-alive connection to `POST /query`.
struct Http {
    r: BufReader<TcpStream>,
    w: TcpStream,
}

impl Http {
    fn open(port: u16) -> Http {
        let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        Http {
            r: BufReader::new(s.try_clone().unwrap()),
            w: s,
        }
    }

    /// `(status, body)`, or `None` once the server is gone.
    fn ask(&mut self, method: &str, path: &str, body: &str) -> Option<(u16, String)> {
        write!(
            self.w,
            "{method} {path} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {TOKEN}\r\n\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .ok()?;
        let mut line = String::new();
        if self.r.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let status = line.split_whitespace().nth(1)?.parse().ok()?;
        let mut len = 0;
        loop {
            let mut h = String::new();
            self.r.read_line(&mut h).ok()?;
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                len = v.trim().parse().ok()?;
            }
        }
        let mut body = vec![0; len];
        self.r.read_exact(&mut body).ok()?;
        Some((status, String::from_utf8_lossy(&body).into_owned()))
    }

    fn query(&mut self, q: &str) -> Option<String> {
        let body = format!("{{\"query\": \"{}\"}}", q.replace('"', "\\\""));
        match self.ask("POST", "/query", &body)? {
            (200, b) => Some(b),
            _ => None,
        }
    }
}

/// One failover: how many writes were acknowledged, how many of those the
/// promoted replica lacks, and how many it holds that were never answered.
fn trial(dir: &Path, sync: &str, run: Duration) -> (usize, usize, usize) {
    let (pf, rf) = (dir.join("p.fenec"), dir.join("r.fenec"));
    let _ = std::fs::remove_file(&pf);
    let _ = std::fs::remove_file(&rf);
    let mut primary = start(&pf, &["--replication-token", TOKEN, "--sync", sync]);
    Http::open(primary.http)
        .query("create collection w (n int @hash)")
        .unwrap();
    let upstream = format!("http://127.0.0.1:{}", primary.http);
    let replica = start(
        &rf,
        &[
            "--replication-token",
            TOKEN,
            "--replica-of",
            &upstream,
            "--sync",
            "250",
        ],
    );

    let acked = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (acked, stop, port) = (Arc::clone(&acked), Arc::clone(&stop), primary.http);
        std::thread::spawn(move || {
            let mut c = Http::open(port);
            let mut n = 0u64;
            while !stop.load(Ordering::SeqCst) {
                if c.query(&format!("put w {{n: {n}}}")).is_none() {
                    break;
                }
                acked.lock().unwrap().push(n);
                n += 1;
            }
        })
    };
    std::thread::sleep(run);
    unsafe { kill(primary.child.id() as i32, 9) };
    let _ = primary.child.wait();
    stop.store(true, Ordering::SeqCst);
    let _ = writer.join();

    // Whatever was on its way has arrived or never will.
    std::thread::sleep(Duration::from_millis(500));
    let mut r = Http::open(replica.http);
    let (status, body) = r.ask("POST", "/_replication/promote", "").unwrap();
    assert_eq!(status, 200, "{body}");
    let rows = r.query("get w select n").unwrap();
    let held: std::collections::HashSet<u64> = rows
        .match_indices("\"n\":")
        .filter_map(|(i, _)| {
            rows[i + 4..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .ok()
        })
        .collect();
    let acked = acked.lock().unwrap().clone();
    let lost = acked.iter().filter(|n| !held.contains(n)).count();
    let unanswered = held.len() + lost - acked.len();
    let mut replica = replica;
    unsafe { kill(replica.child.id() as i32, 15) };
    let _ = replica.child.wait();
    (acked.len(), lost, unanswered)
}

fn main() {
    let trials: usize = std::env::args()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    let dir = std::env::temp_dir().join(format!("fenec-failover-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for sync in ["always", "250"] {
        let mut rows = Vec::new();
        let t = Instant::now();
        for _ in 0..trials {
            rows.push(trial(&dir, sync, Duration::from_millis(2_000)));
        }
        let secs = t.elapsed().as_secs_f64();
        let acked: usize = rows.iter().map(|r| r.0).sum();
        let lost: Vec<usize> = rows.iter().map(|r| r.1).collect();
        let extra: Vec<usize> = rows.iter().map(|r| r.2).collect();
        let rate = acked as f64 / (trials as f64 * 2.0);
        println!(
            "--sync {sync:>6}: {trials} failovers, ~{rate:.0} acknowledged writes/s; \
             acknowledged and lost per failover {lost:?} (the last {:.0} ms of writes at most); \
             held but never answered {extra:?}  [{secs:.1} s]",
            *lost.iter().max().unwrap() as f64 / rate * 1e3,
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
