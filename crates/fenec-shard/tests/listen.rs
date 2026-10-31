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
        let line = lines
            .next()
            .expect("fenec-shard ended before it listened")
            .unwrap();
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

/// A second router over one directory file is refused, naming the first
/// -- as a second `fenec-server --file` is -- and the file is its once the
/// first is gone: two routers, or a shell beside one, wrote over each
/// other's records in it.
#[test]
fn a_second_router_over_the_directory_is_refused() {
    let dir = std::env::temp_dir().join(format!("fenec-shard-held-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("shard.fenec");
    let start = || {
        Command::new(env!("CARGO_BIN_EXE_fenec-shard"))
            .args(["--listen", "127.0.0.1:0", "--directory"])
            .arg(&file)
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    // Its stderr read on, kept open while it runs: a log line written to a
    // pipe closed would be a write refused.
    let listening = |child: &mut std::process::Child| {
        let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
        loop {
            let line = lines.next().expect("fenec-shard ended before it listened");
            if line.unwrap().contains("listening on: http://") {
                return std::thread::spawn(move || lines.for_each(drop));
            }
        }
    };
    let mut first = start();
    let _first_log = listening(&mut first);
    let second = start().wait_with_output().unwrap();
    let said = String::from_utf8_lossy(&second.stderr);
    assert_eq!(second.status.code(), Some(1), "{said}");
    assert!(said.contains("could not open the directory"), "{said}");
    let holder = format!("(held by fenec-shard, pid {})", first.id());
    assert!(said.contains(&holder), "{said}");
    let _ = first.kill();
    let _ = first.wait();
    // Let go with the process, however it ended.
    let mut again = start();
    let _again_log = listening(&mut again);
    let _ = again.kill();
    let _ = again.wait();
    let _ = std::fs::remove_dir_all(&dir);
}
