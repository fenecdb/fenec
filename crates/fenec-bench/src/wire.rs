//! What the wire benches share: fenec-pg started over a file, and its pg
//! wire and HTTP asked by hand (`#[path]`-included by `requests` and
//! `load`).
//!
//! The pg wire is written by hand, text both ways, as psycopg asks. The
//! `postgres` crate binds and reads in binary, and looks up in `pg_type`
//! any type it does not know: fenec-pg sends a parameter it has not typed
//! as OID 0, and the crate's statement for the lookup has its own parameter
//! come back as 0, which it looks up the same way until the stack runs out.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// A connection to fenec-pg's pg wire.
pub struct Wire {
    w: TcpStream,
    r: BufReader<TcpStream>,
    out: Vec<u8>,
    body: Vec<u8>,
}

pub fn cstr(b: &mut Vec<u8>, s: &str) {
    b.extend_from_slice(s.as_bytes());
    b.push(0);
}

impl Wire {
    pub fn connect(addr: &str) -> Wire {
        let w = TcpStream::connect(addr).unwrap();
        w.set_nodelay(true).unwrap();
        let r = BufReader::new(w.try_clone().unwrap());
        let mut c = Wire {
            w,
            r,
            out: Vec::new(),
            body: Vec::new(),
        };
        // The startup packet: its length, protocol 3.0, the user, the
        // database.
        let mut startup = 196_608i32.to_be_bytes().to_vec();
        for s in ["user", "fenec", "database", "fenec", ""] {
            cstr(&mut startup, s);
        }
        let mut packet = ((startup.len() + 4) as i32).to_be_bytes().to_vec();
        packet.extend_from_slice(&startup);
        c.w.write_all(&packet).unwrap();
        c.until_ready();
        c
    }

    /// Queues a message; `send` writes what is queued.
    pub fn msg(&mut self, tag: u8, body: impl FnOnce(&mut Vec<u8>)) {
        self.out.push(tag);
        let at = self.out.len();
        self.out.extend_from_slice(&[0; 4]);
        body(&mut self.out);
        let len = (self.out.len() - at) as i32;
        self.out[at..at + 4].copy_from_slice(&len.to_be_bytes());
    }

    pub fn send(&mut self) {
        self.w.write_all(&self.out).unwrap();
        self.out.clear();
    }

    /// Reads up to the ReadyForQuery; an ErrorResponse is the bench's end.
    pub fn until_ready(&mut self) {
        loop {
            let mut head = [0u8; 5];
            self.r.read_exact(&mut head).unwrap();
            let len = i32::from_be_bytes(head[1..5].try_into().unwrap()) as usize - 4;
            self.body.resize(len, 0);
            self.r.read_exact(&mut self.body).unwrap();
            match head[0] {
                b'E' => panic!("{}", String::from_utf8_lossy(&self.body)),
                b'Z' => return,
                _ => {}
            }
        }
    }

    /// Parses `text` as the statement `name`.
    pub fn prepare(&mut self, name: &str, text: &str) {
        self.msg(b'P', |b| {
            cstr(b, name);
            cstr(b, text);
            b.extend_from_slice(&0i16.to_be_bytes());
        });
        self.msg(b'S', |_| {});
        self.send();
        self.until_ready();
    }

    /// Queues a Bind and an Execute of the statement `name`, every
    /// parameter and column in text.
    pub fn bind_execute(&mut self, name: &str, params: &[&str]) {
        self.msg(b'B', |b| {
            cstr(b, "");
            cstr(b, name);
            b.extend_from_slice(&0i16.to_be_bytes());
            b.extend_from_slice(&(params.len() as i16).to_be_bytes());
            for v in params {
                b.extend_from_slice(&(v.len() as i32).to_be_bytes());
                b.extend_from_slice(v.as_bytes());
            }
            b.extend_from_slice(&0i16.to_be_bytes());
        });
        self.msg(b'E', |b| {
            cstr(b, "");
            b.extend_from_slice(&0i32.to_be_bytes());
        });
    }

    /// Queues a Sync, sends and reads the answers up to its ReadyForQuery.
    pub fn sync(&mut self) {
        self.msg(b'S', |_| {});
        self.send();
        self.until_ready();
    }

    /// A text through the simple protocol.
    pub fn simple(&mut self, text: &str) {
        self.msg(b'Q', |b| cstr(b, text));
        self.send();
        self.until_ready();
    }
}

/// A kept-alive connection to fenec-pg's HTTP.
pub struct Http {
    w: TcpStream,
    r: BufReader<TcpStream>,
    body: Vec<u8>,
}

impl Http {
    pub fn connect(addr: &str) -> Http {
        let w = TcpStream::connect(addr).unwrap();
        w.set_nodelay(true).unwrap();
        let r = BufReader::new(w.try_clone().unwrap());
        Http {
            w,
            r,
            body: Vec::new(),
        }
    }

    /// POSTs `body` to `path` and reads the whole answer; anything but a
    /// 2xx is the bench's end.
    pub fn post(&mut self, path: &str, content_type: &str, body: &[u8]) {
        let head = format!(
            "POST {path} HTTP/1.1\r\nHost: bench\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        self.w.write_all(head.as_bytes()).unwrap();
        self.w.write_all(body).unwrap();
        let mut len = 0usize;
        let mut line = String::new();
        let mut status = String::new();
        loop {
            line.clear();
            self.r.read_line(&mut line).unwrap();
            if status.is_empty() {
                status = line.clone();
            }
            let l = line.trim_end();
            if l.is_empty() {
                break;
            }
            let lower = l.to_ascii_lowercase();
            if let Some(v) = lower.strip_prefix("content-length: ") {
                len = v.parse().unwrap();
            }
        }
        self.body.resize(len, 0);
        self.r.read_exact(&mut self.body).unwrap();
        assert!(
            status.contains(" 200 ") || status.contains(" 201 "),
            "{status} {}",
            String::from_utf8_lossy(&self.body)
        );
    }
}

pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// fenec-pg, killed when dropped.
pub struct Server(Child);
impl Server {
    /// Its process id, to read its resident set by.
    #[allow(dead_code)]
    pub fn pid(&self) -> u32 {
        self.0.id()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// fenec-pg over `file`, its pg wire on `pg` and HTTP on `http`, answering
/// both; `target` names the make target that builds it.
pub fn start_fenec(file: &Path, pg: u16, http: u16, target: &str) -> Server {
    let bin = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/release/fenec-pg");
    let child = Command::new(&bin)
        .args([
            "--listen",
            &format!("127.0.0.1:{pg}"),
            "--http",
            &format!("127.0.0.1:{http}"),
        ])
        .args(["--file", file.to_str().unwrap(), "--no-checkpoint"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("{}: {e} -- make {target} builds it", bin.display()));
    let server = Server(child);
    let until = Instant::now() + Duration::from_secs(30);
    while TcpStream::connect(("127.0.0.1", http)).is_err()
        || TcpStream::connect(("127.0.0.1", pg)).is_err()
    {
        assert!(Instant::now() < until, "fenec-pg did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    server
}
