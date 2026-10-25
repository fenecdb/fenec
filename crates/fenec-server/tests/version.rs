//! `fenec-server --version` and `-V`: the line a package's check and a
//! script read, on stdout, and nothing opened. Before them the version was
//! the first line of `--help`, on stderr, and `--version` an unknown option.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_fenec-server");

/// A directory of its own, removed at the end.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let d = std::env::temp_dir().join(format!(
            "fenec-server-version-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Dir(d)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn server(args: &[&str]) -> Output {
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
    let want = format!("fenec-server {}\n", env!("CARGO_PKG_VERSION"));
    for flag in ["--version", "-V"] {
        let out = server(&[flag]);
        assert_eq!(out.status.code(), Some(0), "{flag}: {}", text(&out.stderr));
        assert_eq!(text(&out.stdout), want, "{flag}");
        assert_eq!(text(&out.stderr), "", "{flag}");
    }
}

#[test]
fn a_version_among_the_flags_opens_and_binds_nothing() {
    // Wherever a flag stands, as `--help` is read; a port held here would
    // fail a server that tried to bind it, and the file is never made.
    let dir = Dir::new("among");
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = held.local_addr().unwrap().to_string();
    let file = dir.0.join("data.fenec");
    let file = file.to_str().unwrap();
    let want = format!("fenec-server {}\n", env!("CARGO_PKG_VERSION"));
    for args in [
        &["--file", file, "--http", &port, "--version"][..],
        &["--dir", file, "-V", "--http", &port][..],
    ] {
        let out = server(args);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{args:?}: {}",
            text(&out.stderr)
        );
        assert_eq!(text(&out.stdout), want, "{args:?}");
    }
    let left: Vec<_> = std::fs::read_dir(&dir.0).unwrap().collect();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn a_flags_value_is_not_the_flag() {
    // `--http-token -V` is a token that reads "-V", not a version asked
    // for: the server goes on to its start, here refused by the flag after.
    let out = server(&["--http-token", "-V", "--no-such-flag"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "");
    assert!(
        text(&out.stderr).contains("unknown option: --no-such-flag"),
        "{}",
        text(&out.stderr)
    );
}
