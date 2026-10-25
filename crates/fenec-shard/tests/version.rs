//! `fenec-shard --version` and `-V`: the line a script reads, on stdout,
//! and nothing opened. Before them the router had no version flag; its
//! version was the first line of `--help`, on stderr.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_fenec-shard");

fn shard(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn version_and_v_print_the_name_and_version_on_stdout() {
    let want = format!("fenec-shard {}\n", env!("CARGO_PKG_VERSION"));
    for flag in ["--version", "-V"] {
        let out = shard(&[flag]);
        assert_eq!(out.status.code(), Some(0), "{flag}: {}", text(&out.stderr));
        assert_eq!(text(&out.stdout), want, "{flag}");
        assert_eq!(text(&out.stderr), "", "{flag}");
    }
}

#[test]
fn a_version_among_the_flags_opens_no_directory() {
    // Wherever a flag stands, as `--help` is read: the directory file a
    // router would open (and make) is never made, and the port it names,
    // held here, is never asked for.
    let dir: PathBuf =
        std::env::temp_dir().join(format!("fenec-shard-version-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = held.local_addr().unwrap().to_string();
    let directory = dir.join("shard.fenec");
    let out = shard(&[
        "--listen",
        &port,
        "--directory",
        directory.to_str().unwrap(),
        "--version",
    ]);
    let left: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        format!("fenec-shard {}\n", env!("CARGO_PKG_VERSION"))
    );
    assert!(left.is_empty(), "{left:?}");
}
