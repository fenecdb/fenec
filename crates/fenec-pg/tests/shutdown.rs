//! The shutdown path -- against a real process.
//!
//! These behaviours cannot be exercised *inside* the process: shutdown ends
//! in `process::exit` and the shutdown flag belongs to the whole process.
//! So the test runs the `fenec-pg` binary, sends SIGTERM and inspects the file
//! left behind -- exactly what `docker stop` does.

use fenec_pg::client::{Client, Url};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Stdio};

// libc's `kill`; `server.rs` declares `signal` the same way (to avoid
// adding a dependency).
extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}
const SIGTERM: i32 = 15;

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fenecpg-shutdown-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let _ = std::fs::remove_file(&path);
    path
}

struct Running {
    child: Child,
    port: u16,
    err: BufReader<ChildStderr>,
}

/// Starts the binary on a random port and waits for it to begin listening.
fn start(path: &Path, extra: &[&str]) -> Running {
    let mut child = Command::new(env!("CARGO_BIN_EXE_fenec-pg"))
        .arg("--listen")
        .arg("127.0.0.1:0")
        .arg("--file")
        .arg(path)
        .args(extra)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("could not start fenec-pg");

    // "fenec-pg 0.1.0 listening on: postgres://localhost:54321/fenec  [...]"
    let mut err = BufReader::new(child.stderr.take().unwrap());
    let mut port = None;
    for _ in 0..10 {
        let mut line = String::new();
        if err.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        if let Some(rest) = line.split("localhost:").nth(1) {
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            port = digits.parse().ok();
            break;
        }
    }
    let port = port.expect("the listening port was not found in stderr");
    Running { child, port, err }
}

impl Running {
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

    /// Sends SIGTERM and waits for the process to end; exit code + stderr.
    fn terminate(mut self) -> (i32, String) {
        unsafe { kill(self.child.id() as i32, SIGTERM) };
        let status = self.child.wait().expect("could not wait for the process");
        let mut rest = String::new();
        self.err.read_to_string(&mut rest).unwrap_or_default();
        (status.code().unwrap_or(-1), rest)
    }
}

fn documents(path: &Path, collection: &str) -> usize {
    let db = fenec_core::fs::open(path).expect("could not reopen the file");
    db.stats()
        .iter()
        .find(|s| s.name == collection)
        .map(|s| s.documents)
        .unwrap_or(0)
}

/// `--sync off`: nothing reaches the disk except on shutdown. So every row
/// found in the file went through the shutdown path -- a write lost between
/// `sync` and `exit` shows up as missing here.
#[test]
fn sigterm_flushes_pending_writes() {
    let path = tmp("drain.fenec");
    let server = start(&path, &["--sync", "off"]);

    let mut c = server.client();
    c.query("create collection t (name text)").unwrap();
    c.query(r#"put t {name: "one"}"#).unwrap();
    c.query(r#"put t {name: "two"}"#).unwrap();

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
        let path = tmp(if i == 0 {
            "cp-on.fenec"
        } else {
            "cp-off.fenec"
        });
        let server = start(&path, extra);
        let mut c = server.client();
        c.query("create collection t (name text, e vector<4> @hnsw(cosine))")
            .unwrap();
        c.query(&put).unwrap();

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
