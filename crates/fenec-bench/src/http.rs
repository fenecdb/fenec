//! What the server benches share: fenec-server started over a file, and
//! its HTTP asked by hand on a kept-alive connection (`#[path]`-included by
//! `requests`, `load` and `scale`).
//!
//! The client is written here rather than taken from a crate: a request is
//! its head and body in one write, an answer its head and a body of the
//! length it names, and that is all a bench needs to measure the server
//! rather than a client's machinery. One connection a client thread, used
//! for every request it sends, as a pool hands one out.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// A kept-alive connection to fenec-server's HTTP.
pub struct Http {
    w: TcpStream,
    r: BufReader<TcpStream>,
    head: Vec<u8>,
    body: Vec<u8>,
    line: String,
}

impl Http {
    pub fn connect(addr: &str) -> Http {
        let w = TcpStream::connect(addr).unwrap();
        // A request goes out in one write, and waiting for an ACK to send
        // the next one would be Nagle's 40 ms, not the server's time.
        w.set_nodelay(true).unwrap();
        let r = BufReader::with_capacity(1 << 16, w.try_clone().unwrap());
        Http {
            w,
            r,
            head: Vec::with_capacity(256),
            body: Vec::new(),
            line: String::new(),
        }
    }

    /// Sends one request and reads the whole answer: its status and body.
    pub fn request(
        &mut self,
        method: &str,
        path: &str,
        content_type: &str,
        body: &[u8],
    ) -> (u16, &[u8]) {
        self.head.clear();
        write!(
            self.head,
            "{method} {path} HTTP/1.1\r\nHost: bench\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .unwrap();
        // Head and body in one write when the body is small: two writes on
        // a socket that sends at once are two packets, and the server reads
        // the head before it waits for the rest.
        if body.len() <= 64 << 10 {
            self.head.extend_from_slice(body);
            self.w.write_all(&self.head).unwrap();
        } else {
            self.w.write_all(&self.head).unwrap();
            self.w.write_all(body).unwrap();
        }
        let status = self.read_answer();
        (status, &self.body)
    }

    /// POSTs `body` to `path`; anything but a 2xx is the bench's end.
    pub fn post(&mut self, path: &str, content_type: &str, body: &[u8]) -> &[u8] {
        let (status, out) = self.request("POST", path, content_type, body);
        assert!(
            (200..300).contains(&status),
            "POST {path}: {status} {}",
            String::from_utf8_lossy(out)
        );
        out
    }

    /// `query` with its time split, in ns: the request's write, the wait
    /// for the answer's first bytes, and the rest of it read.
    pub fn query_timed(&mut self, text: &str, params: &str) -> [u64; 3] {
        let body = format!("{{\"query\":{},\"params\":[{params}]}}", json_string(text));
        self.head.clear();
        write!(
            self.head,
            "POST /query HTTP/1.1\r\nHost: bench\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .unwrap();
        self.head.extend_from_slice(body.as_bytes());
        let t0 = Instant::now();
        self.w.write_all(&self.head).unwrap();
        let t1 = Instant::now();
        self.r.fill_buf().unwrap();
        let t2 = Instant::now();
        let status = self.read_answer();
        let t3 = Instant::now();
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&self.body));
        [
            (t1 - t0).as_nanos() as u64,
            (t2 - t1).as_nanos() as u64,
            (t3 - t2).as_nanos() as u64,
        ]
    }

    /// `POST /query` of `text` with `params`, a JSON array's insides.
    pub fn query(&mut self, text: &str, params: &str) -> &[u8] {
        let body = format!("{{\"query\":{},\"params\":[{params}]}}", json_string(text));
        self.post("/query", "application/json", body.as_bytes())
    }

    /// The last answer's body, kept until the next request.
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// GETs `path`; anything but a 2xx is the bench's end.
    pub fn get(&mut self, path: &str) -> &[u8] {
        let (status, out) = self.request("GET", path, "text/plain", b"");
        assert!(
            (200..300).contains(&status),
            "GET {path}: {status} {}",
            String::from_utf8_lossy(out)
        );
        out
    }

    /// The status line, the headers, and a body of `Content-Length` bytes or
    /// in chunks, into `self.body`.
    fn read_answer(&mut self) -> u16 {
        let mut status = 0u16;
        let mut len = 0usize;
        let mut chunked = false;
        loop {
            self.line.clear();
            let n = self.r.read_line(&mut self.line).unwrap();
            assert!(n > 0, "the server closed the connection");
            let l = self.line.trim_end();
            if status == 0 {
                status = l
                    .split_whitespace()
                    .nth(1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or_else(|| panic!("not an HTTP status line: {l}"));
                continue;
            }
            if l.is_empty() {
                break;
            }
            let Some((k, v)) = l.split_once(':') else {
                continue;
            };
            let v = v.trim();
            if k.eq_ignore_ascii_case("content-length") {
                len = v.parse().unwrap();
            } else if k.eq_ignore_ascii_case("transfer-encoding") {
                chunked = v.eq_ignore_ascii_case("chunked");
            }
        }
        self.body.clear();
        if !chunked {
            self.body.resize(len, 0);
            self.r.read_exact(&mut self.body).unwrap();
            return status;
        }
        loop {
            self.line.clear();
            self.r.read_line(&mut self.line).unwrap();
            let size = usize::from_str_radix(self.line.trim_end(), 16).unwrap();
            let at = self.body.len();
            self.body.resize(at + size, 0);
            self.r.read_exact(&mut self.body[at..]).unwrap();
            self.line.clear();
            self.r.read_line(&mut self.line).unwrap();
            if size == 0 {
                return status;
            }
        }
    }
}

/// `s` as a JSON string.
pub fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The `id`s of an answer's rows, in their order: `[{"id":3,..},..]`.
pub fn ids(body: &[u8]) -> Vec<i64> {
    let text = std::str::from_utf8(body).unwrap();
    text.match_indices("\"id\":")
        .filter_map(|(at, key)| {
            let rest = &text[at + key.len()..];
            let end = rest
                .find(|c: char| !c.is_ascii_digit() && c != '-')
                .unwrap_or(rest.len());
            rest[..end].parse().ok()
        })
        .collect()
}

/// A vector as JSON's array of its numbers, each the shortest text that
/// reads back as the `f32`, as a client's JSON encoder writes it.
pub fn vector_json(v: &[f32]) -> String {
    let mut s = String::with_capacity(v.len() * 12);
    s.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        write_f32(&mut s, *x);
    }
    s.push(']');
    s
}

fn write_f32(s: &mut String, x: f32) {
    use std::fmt::Write as _;
    let _ = write!(s, "{x}");
}

pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// fenec-server, killed when dropped.
pub struct Server(Child);
impl Server {
    /// A server started elsewhere, killed when dropped as one this module
    /// started.
    pub fn from_child(child: Child) -> Server {
        Server(child)
    }

    /// Its process id, to read its resident set by.
    pub fn pid(&self) -> u32 {
        self.0.id()
    }

    /// Stops it as a supervisor does, with SIGTERM, and waits for it: its
    /// last writes are synced on the way down, where a kill under `--sync
    /// <ms>` loses those of the last interval.
    pub fn terminate(mut self) {
        let _ = Command::new("kill")
            .args(["-TERM", &self.0.id().to_string()])
            .status();
        let _ = self.0.wait();
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// fenec-server over `file`, its HTTP on `http`, with `extra` flags;
/// `target` names the make target that builds it.
pub fn start_fenec(file: &Path, http: u16, target: &str, extra: &[&str]) -> Server {
    let bin = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/release/fenec-server");
    let child = Command::new(&bin)
        .args(["--http", &format!("127.0.0.1:{http}")])
        .args(["--file", file.to_str().unwrap(), "--no-checkpoint"])
        .args(extra)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("{}: {e} -- make {target} builds it", bin.display()));
    let server = Server(child);
    let until = Instant::now() + Duration::from_secs(120);
    while TcpStream::connect(("127.0.0.1", http)).is_err() {
        assert!(Instant::now() < until, "fenec-server did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    server
}
