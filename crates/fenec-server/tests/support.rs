//! A `fenec-server` process for a test, and an HTTP client to talk to it.
//!
//! What these tests hold -- a shutdown's sync, a crash's file, a replica's
//! promotion -- cannot be exercised inside the test's own process: a
//! shutdown ends in `process::exit` and its flag is the whole process's.
//! So the binary runs, on a port it picks, and is asked over HTTP as any
//! client asks it.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// libc's `kill`, declared as the server declares `signal`: no dependency.
extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}
pub const SIGKILL: i32 = 9;
pub const SIGTERM: i32 = 15;

/// A fresh path under a directory of this test process's own.
pub fn tmp(tag: &str, name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fenec-server-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&path);
    path
}

pub struct Server {
    pub child: Child,
    pub port: u16,
    pub log: Arc<Mutex<String>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Starts the binary with `args`, on a port of its own unless `args` name
/// one with `--http`, and waits for its HTTP listener; everything it logs
/// is kept.
pub fn start(args: &[&str]) -> Server {
    try_start(args).unwrap_or_else(|log| panic!("fenec-server ended before it listened:\n{log}"))
}

/// [`start`], or what the server logged before it ended without
/// listening.
pub fn try_start(args: &[&str]) -> Result<Server, String> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_fenec-server"));
    if !args.contains(&"--http") {
        cmd.args(["--http", "127.0.0.1:0"]);
    }
    let mut child = cmd
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("could not start fenec-server");
    let mut err = BufReader::new(child.stderr.take().unwrap());
    let log = Arc::new(Mutex::new(String::new()));
    // "fenec-http 0.1.6 listening on: http://127.0.0.1:54321  [no auth]"
    let mut port = None;
    while port.is_none() {
        let mut line = String::new();
        if err.read_line(&mut line).unwrap_or(0) == 0 {
            let _ = child.wait();
            let log = log.lock().unwrap().clone();
            return Err(log);
        }
        if line.contains("listening on: http://") {
            if let Some(rest) = line.rsplit_once("http://").map(|(_, r)| r) {
                let addr: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
                port = addr.rsplit_once(':').and_then(|(_, p)| p.parse().ok());
            }
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
    Ok(Server {
        child,
        port: port.unwrap(),
        log,
    })
}

impl Server {
    pub fn http(&self) -> Http {
        Http::open(self.port)
    }

    pub fn logged(&self, what: &str, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if self.log.lock().unwrap().contains(what) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// Sends `sig`, waits for the process to end; its exit code and
    /// everything it logged.
    pub fn signal(mut self, sig: i32) -> (i32, String) {
        unsafe { kill(self.child.id() as i32, sig) };
        let status = self.child.wait().expect("could not wait for the process");
        // The reader thread takes the last lines as the pipe closes.
        std::thread::sleep(Duration::from_millis(100));
        let log = self.log.lock().unwrap().clone();
        (status.code().unwrap_or(-1), log)
    }

    pub fn terminate(self) -> (i32, String) {
        self.signal(SIGTERM)
    }
}

/// A keep-alive HTTP/1.1 connection.
pub struct Http {
    r: BufReader<TcpStream>,
    w: TcpStream,
    /// Sent with every request: `Authorization: Bearer ...`, a tenant's
    /// prefix is the target's.
    pub token: Option<String>,
}

pub struct Answer {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Answer {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

impl Http {
    pub fn open(port: u16) -> Http {
        let s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        Http {
            r: BufReader::new(s.try_clone().unwrap()),
            w: s,
            token: None,
        }
    }

    pub fn with_token(mut self, token: &str) -> Http {
        self.token = Some(token.to_string());
        self
    }

    /// One request; `None` when the server closed the connection.
    pub fn try_ask(
        &mut self,
        method: &str,
        target: &str,
        body: &str,
        headers: &[(&str, &str)],
    ) -> Option<Answer> {
        let mut req = format!("{method} {target} HTTP/1.1\r\nHost: t\r\n");
        if let Some(t) = &self.token {
            req.push_str(&format!("Authorization: Bearer {t}\r\n"));
        }
        for (k, v) in headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
        self.w.write_all(req.as_bytes()).ok()?;
        let mut line = String::new();
        if self.r.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let status = line.split_whitespace().nth(1)?.parse().ok()?;
        let mut len = 0;
        let mut out = Vec::new();
        loop {
            let mut h = String::new();
            self.r.read_line(&mut h).ok()?;
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                if k.eq_ignore_ascii_case("content-length") {
                    len = v.trim().parse().ok()?;
                }
                out.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
        let mut body = vec![0; len];
        self.r.read_exact(&mut body).ok()?;
        Some(Answer {
            status,
            headers: out,
            body: String::from_utf8_lossy(&body).into_owned(),
        })
    }

    pub fn ask(&mut self, method: &str, target: &str, body: &str) -> Answer {
        self.try_ask(method, target, body, &[])
            .expect("the server closed the connection")
    }

    /// [`Http::ask`] with headers of its own.
    pub fn ask_with(
        &mut self,
        method: &str,
        target: &str,
        body: &str,
        headers: &[(&str, &str)],
    ) -> Answer {
        self.try_ask(method, target, body, headers)
            .expect("the server closed the connection")
    }

    /// A statement through `POST <prefix>/query`: its body, or the status
    /// and body it was refused with.
    pub fn query_at(&mut self, prefix: &str, q: &str) -> Result<String, (u16, String)> {
        let body = format!("{{\"query\": {}}}", json_string(q));
        let a = self.ask("POST", &format!("{prefix}/query"), &body);
        match a.status {
            200 => Ok(a.body),
            s => Err((s, a.body)),
        }
    }

    pub fn query(&mut self, q: &str) -> Result<String, (u16, String)> {
        self.query_at("", q)
    }

    /// The statement, which must be answered.
    pub fn run(&mut self, q: &str) -> String {
        self.query(q).unwrap_or_else(|e| panic!("{q}: {e:?}"))
    }
}

pub fn json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The values of `field` in a JSON answer's rows, as text, in order.
pub fn column(body: &str, field: &str) -> Vec<String> {
    let key = format!("\"{field}\":");
    body.match_indices(&key)
        .map(|(i, _)| {
            let rest = &body[i + key.len()..];
            let rest = rest.trim_start();
            if let Some(s) = rest.strip_prefix('"') {
                s[..s.find('"').unwrap_or(s.len())].to_string()
            } else {
                rest.chars()
                    .take_while(|c| !matches!(c, ',' | '}' | ']'))
                    .collect::<String>()
                    .trim()
                    .to_string()
            }
        })
        .collect()
}

/// The `count` a `get ... count` answered.
pub fn count(body: &str) -> i64 {
    column(body, "count")
        .first()
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no count in {body}"))
}

/// Documents in `collection` of the file at `path`, opened on its own as
/// after a crash.
pub fn documents(path: &std::path::Path, collection: &str) -> usize {
    let db = fenec_core::fs::open(path).expect("could not reopen the file");
    db.stats()
        .iter()
        .find(|s| s.name == collection)
        .map(|s| s.documents)
        .unwrap_or(0)
}
