//! The connections a server answered as it took them, counted by why:
//! `fenec_connections_refused_total{reason}`. A binary of its own: the
//! counts are the process's, and the other tests' servers refuse too.

use fenec_core::prelude::*;
use fenec_http::{Config, Server};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// A keep-alive connection, the scrapes asked over it: past the ceiling,
/// a new connection would be refused, the scrape's own among them.
struct Kept {
    r: BufReader<TcpStream>,
    w: TcpStream,
}

impl Kept {
    fn open(port: u16) -> Kept {
        let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        Kept {
            r: BufReader::new(s.try_clone().unwrap()),
            w: s,
        }
    }

    /// `GET target`'s status and body.
    fn get(&mut self, target: &str) -> (u16, String) {
        write!(self.w, "GET {target} HTTP/1.1\r\nHost: t\r\n\r\n").unwrap();
        let mut line = String::new();
        self.r.read_line(&mut line).unwrap();
        let status = line.split_whitespace().nth(1).unwrap().parse().unwrap();
        let mut length = 0;
        loop {
            line.clear();
            self.r.read_line(&mut line).unwrap();
            let l = line.trim_end();
            if l.is_empty() {
                break;
            }
            if let Some((k, v)) = l.split_once(':') {
                if k.eq_ignore_ascii_case("content-length") {
                    length = v.trim().parse().unwrap();
                }
            }
        }
        let mut body = vec![0u8; length];
        self.r.read_exact(&mut body).unwrap();
        (status, String::from_utf8(body).unwrap())
    }

    /// The count of connections refused for `reason`.
    fn refused(&mut self, reason: &str) -> u64 {
        let (status, body) = self.get("/_metrics");
        assert_eq!(status, 200, "{body}");
        let line = format!("fenec_connections_refused_total{{reason=\"{reason}\"}} ");
        body.lines()
            .find_map(|l| l.strip_prefix(line.as_str()))
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("no {line}in {body}"))
    }
}

/// A connection of its own, its request sent: the answer's status line.
fn refused_with(port: u16) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(b"GET /collections HTTP/1.1\r\nHost: t\r\n\r\n")
        .unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out
}

/// A connection given no thread counts under `no_thread`, one past the
/// ceiling under `ceiling`, each by one and the other not at all.
#[test]
fn a_refused_connection_is_counted_by_why() {
    fenec_http::audit::set_delay(0);
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        max_connections: 2,
        ..Config::default()
    };
    let server = Server::new(Arc::new(RwLock::new(Database::new())), cfg);
    let fail = server.fail_spawns();
    let listener = server.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    let mut scrape = Kept::open(port);
    let (ceiling, no_thread) = (scrape.refused("ceiling"), scrape.refused("no_thread"));

    // A second connection is within the ceiling of two: its thread is all
    // it lacks.
    fail.next(1);
    let out = refused_with(port);
    assert!(out.starts_with("HTTP/1.1 503 "), "{out}");
    assert!(out.contains("\r\nFenec-Unserved: no_thread\r\n"), "{out}");
    assert_eq!(scrape.refused("no_thread"), no_thread + 1);
    assert_eq!(scrape.refused("ceiling"), ceiling);

    // Two connections served, and the third past the ceiling.
    let mut second = Kept::open(port);
    assert_eq!(second.get("/collections").0, 200);
    let out = refused_with(port);
    assert!(out.starts_with("HTTP/1.1 503 "), "{out}");
    assert!(out.contains("\r\nFenec-Unserved: ceiling\r\n"), "{out}");
    assert!(out.contains("too many connections"), "{out}");
    assert_eq!(scrape.refused("ceiling"), ceiling + 1);
    assert_eq!(scrape.refused("no_thread"), no_thread + 1);
}
