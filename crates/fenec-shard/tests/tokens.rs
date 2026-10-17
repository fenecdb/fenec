//! The router's tokens from the environment, as deploy/'s systemd units
//! hand them over (`EnvironmentFile=`) rather than on a command line every
//! user of the machine reads with `ps`: FENEC_SHARD_TOKEN, the directory's
//! FENEC_SHARD_REPLICATION_TOKEN and FENEC_SHARD_ENTRY_TOKEN. Set and empty
//! is unset, as an environment file leaves a line it does not use: taken
//! for a token, an empty FENEC_SHARD_TOKEN let a request carrying no
//! `Authorization` into /_shard/ and passed the check that keeps a router
//! with no token off a public address.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fenec-shard-tokens-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// The router listening, and where; or everything it said before it ended.
fn router(listen: &str, dir: &Path, env: &[(&str, &str)]) -> Result<(Child, String), String> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_fenec-shard"))
        .env_remove("FENEC_SHARD_TOKEN")
        .env_remove("FENEC_SHARD_ENTRY_TOKEN")
        .env_remove("FENEC_SHARD_REPLICATION_TOKEN")
        .envs(env.iter().copied())
        .args(["--listen", listen, "--auth-delay", "0", "--directory"])
        .arg(dir.join("shard.fenec"))
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut said = String::new();
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    loop {
        let Some(Ok(line)) = lines.next() else {
            let _ = child.wait();
            return Err(said);
        };
        if let Some((_, rest)) = line.split_once("listening on: http://") {
            let addr = rest.split_whitespace().next().unwrap().to_string();
            return Ok((child, addr));
        }
        said.push_str(&line);
        said.push('\n');
    }
}

fn status(addr: &str, target: &str, token: Option<&str>) -> u16 {
    let mut s = TcpStream::connect(addr).unwrap();
    let auth = token.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    write!(
        s,
        "GET {target} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n{auth}\r\n"
    )
    .unwrap();
    let mut answer = String::new();
    s.read_to_string(&mut answer).unwrap();
    answer[9..12].parse().unwrap()
}

#[test]
fn a_token_set_empty_is_no_token() {
    // On an address past loopback a router with no token is refused, and
    // an empty FENEC_SHARD_TOKEN is none: nothing listens.
    let dir = scratch("empty-env");
    match router("0.0.0.0:0", &dir, &[("FENEC_SHARD_TOKEN", "")]) {
        Ok((mut child, addr)) => {
            let open = status(&addr.replace("0.0.0.0", "127.0.0.1"), "/_shard/nodes", None);
            let _ = child.kill();
            let _ = child.wait();
            panic!("an empty FENEC_SHARD_TOKEN listened on {addr}, /_shard/ answering {open} to no token");
        }
        Err(said) => assert!(said.contains("not a loopback address"), "{said}"),
    }
    // Given as a flag, refused at the start: the router's token, and the
    // directory's replication token, which empty turned replication off
    // unsaid.
    for flag in ["--token", "--replication-token", "--entry-token"] {
        let dir = scratch("empty-flag");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_fenec-shard"));
        let out = cmd
            .env_remove("FENEC_SHARD_TOKEN")
            .env_remove("FENEC_SHARD_REPLICATION_TOKEN")
            .env_remove("FENEC_SHARD_ENTRY_TOKEN")
            .args(["--listen", "127.0.0.1:0", flag, "", "--directory"])
            .arg(dir.join("shard.fenec"))
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{flag}: {said}");
        assert!(said.contains(&format!("{flag} is empty")), "{flag}: {said}");
    }
}

#[test]
fn an_embedders_empty_token_is_no_token() {
    // `Router::new` holds a `Config` to the binary's rule: an empty token
    // is none, so a router with one is refused an address past loopback
    // as a router with none is -- where it listened, and its `/_shard/`
    // answered a request carrying no `Authorization`.
    let router = fenec_shard::Router::new(
        fenec_shard::directory::Directory::in_memory(),
        fenec_shard::Config {
            addr: "0.0.0.0:0".into(),
            token: Some(String::new()),
            ..fenec_shard::Config::default()
        },
    );
    let bound = router.bind();
    assert!(
        bound.is_err(),
        "an empty token listened on {:?}",
        bound.map(|l| l.local_addr())
    );
}

#[test]
fn the_router_takes_its_tokens_from_the_environment() {
    let dir = scratch("env");
    let (mut child, addr) = router(
        "127.0.0.1:0",
        &dir,
        &[
            ("FENEC_SHARD_TOKEN", "root-from-env"),
            ("FENEC_SHARD_REPLICATION_TOKEN", "dir-from-env"),
        ],
    )
    .unwrap_or_else(|said| panic!("the router did not start:\n{said}"));
    let asked = [
        status(&addr, "/_shard/nodes", None),
        status(&addr, "/_shard/nodes", Some("root-from-env")),
        status(&addr, "/_replication/status", None),
        status(&addr, "/_replication/status", Some("dir-from-env")),
    ];
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(asked, [401, 200, 401, 200]);

    // The entry's token is held apart from the replication token wherever
    // each comes from.
    let dir = scratch("entry");
    let said = router(
        "127.0.0.1:0",
        &dir,
        &[
            ("FENEC_SHARD_TOKEN", "root-from-env"),
            ("FENEC_SHARD_REPLICATION_TOKEN", "shared"),
            ("FENEC_SHARD_ENTRY_TOKEN", "shared"),
        ],
    )
    .map(|(mut child, _)| {
        let _ = child.kill();
        let _ = child.wait();
    })
    .expect_err("an entry token equal to the replication token was taken");
    assert!(
        said.contains("--entry-token is --replication-token"),
        "{said}"
    );
}
