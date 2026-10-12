//! The binary on port 0 says where it listens. Every harness that starts a
//! router -- the examples' clusters, the studio's tests -- asks for a port
//! the system picks rather than a fixed one, which met "Address already in
//! use" beside any other suite on the machine, and reads the port off this
//! line: it has to name the port bound, not the 0 asked for.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};

#[test]
fn a_router_on_port_0_names_the_port_it_took() {
    let dir = std::env::temp_dir().join(format!("fenec-shard-listen-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_fenec-shard"))
        .args(["--listen", "127.0.0.1:0", "--directory"])
        .arg(dir.join("shard.fenec"))
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    // "fenec-shard 0.1.10 listening on: http://127.0.0.1:54321  [0 node(s), 0 tenant(s)]"
    let addr = loop {
        let line = lines.next().expect("fenec-shard ended before it listened").unwrap();
        if let Some((_, rest)) = line.split_once("listening on: http://") {
            break rest.split_whitespace().next().unwrap().to_string();
        }
    };
    let port: u16 = addr.rsplit_once(':').unwrap().1.parse().unwrap();
    assert_ne!(port, 0, "{addr}");

    // And it is the router there: its directory, empty, with no token asked.
    let mut s = TcpStream::connect(&addr).unwrap();
    s.write_all(b"GET /_shard/nodes HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut answer = String::new();
    s.read_to_string(&mut answer).unwrap();
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert!(answer.ends_with("[]"), "{answer}");

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}
