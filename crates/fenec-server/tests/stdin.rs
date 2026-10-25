//! `--exit-on-stdin-close`: the end of stdin is a SIGTERM.
//!
//! An app holding a pipe on its server's stdin is killed, and the kernel
//! closes the pipe's end; the server must then stop as a SIGTERM stops it
//! -- every write it answered on disk, `<file>.lock` let go, exit 0 --
//! rather than run on with nobody to answer, holding the file. Without the
//! flag stdin is never read, so a server started with whatever stdin a
//! shell gave it goes on as it did.

use crate::support::{count, start, start_piped, tmp};
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Long enough for a loaded machine's shutdown; an exit is waited for, not
/// slept through.
const EXIT: Duration = Duration::from_secs(30);

/// Rows in `collection` of the file at `path`, read without writing to it:
/// the server may still be serving it.
fn rows(path: &std::path::Path, collection: &str) -> usize {
    let db = fenec_core::fs::open_read_only(path).expect("could not read the file");
    db.stats()
        .iter()
        .find(|s| s.name == collection)
        .map(|s| s.documents)
        .unwrap_or(0)
}

/// `--sync off`: nothing reaches the disk but at shutdown, so each row in
/// the file went through the shutdown the end of stdin started.
#[test]
fn the_end_of_stdin_shuts_the_server_down_as_sigterm_does() {
    let path = tmp("stdin", "closed.fenec");
    let p = path.to_str().unwrap();
    let mut server = start_piped(&["--file", p, "--sync", "off", "--exit-on-stdin-close"]);
    let mut c = server.http();
    c.run("create collection t (name text)");
    c.run(r#"put t {name: "one"}"#);

    // What a parent writes is read and dropped: a mebibyte through a pipe
    // that holds 64 KB at most is written only as the server reads it, and
    // the server takes it for no end of its input.
    let mut stdin = server.child.stdin.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let written = stdin
            .write_all(&vec![b'x'; 1 << 20])
            .and_then(|_| stdin.flush());
        let _ = tx.send(written.map(|_| stdin));
    });
    let stdin = rx
        .recv_timeout(EXIT)
        .expect("the server did not read its stdin")
        .expect("the pipe broke under the write");
    c.run(r#"put t {name: "two"}"#);
    assert!(server.child.try_wait().unwrap().is_none());

    drop(stdin);
    let (code, log) = server
        .exited(EXIT)
        .expect("the server ran on once its stdin closed");
    assert_eq!(code, 0, "expected a clean exit\n{log}");
    assert!(log.contains("stdin closed: shutting down"), "{log}");
    assert!(
        log.contains("shutting down: writes were pushed to disk"),
        "the shutdown's sync did not run\n{log}"
    );
    assert_eq!(rows(&path, "t"), 2, "a write was lost\n{log}");

    // `<file>.lock` is free: another server opens the file and reads it.
    let again = start(&["--file", p]);
    assert_eq!(count(&again.http().run("get t count")), 2);
}

/// A node of tenants goes down the same way: every open tenant synced.
#[test]
fn a_node_of_tenants_shuts_down_at_the_end_of_stdin_too() {
    let dir = tmp("stdin", "tenants");
    std::fs::create_dir_all(&dir).unwrap();
    let d = dir.to_str().unwrap();
    let mut server = start_piped(&[
        "--dir",
        d,
        "--admin-token",
        "admin",
        "--sync",
        "off",
        "--exit-on-stdin-close",
    ]);
    let admin = server
        .http()
        .with_token("admin")
        .ask("PUT", "/_admin/tenants/acme", "");
    assert_eq!(admin.status, 201, "{}", admin.body);
    let mut c = server.http();
    for q in ["create collection t (name text)", r#"put t {name: "one"}"#] {
        if let Err(e) = c.query_at("/t/acme", q) {
            panic!("{q}: {e:?}");
        }
    }

    drop(server.child.stdin.take());
    let (code, log) = server
        .exited(EXIT)
        .expect("the node ran on once its stdin closed");
    assert_eq!(code, 0, "expected a clean exit\n{log}");
    assert!(log.contains("1 open tenant(s) synced"), "{log}");
    assert_eq!(
        rows(&dir.join("acme.fenec"), "t"),
        1,
        "a write was lost\n{log}"
    );
}

/// Without the flag a closed stdin is nothing: the server is the one a
/// shell or a supervisor started with whatever stdin it had, and goes on.
#[test]
fn without_the_flag_the_end_of_stdin_is_nothing() {
    let path = tmp("stdin", "kept.fenec");
    let mut kept = start_piped(&["--file", path.to_str().unwrap(), "--sync", "50"]);
    let other = tmp("stdin", "beside.fenec");
    let mut closed = start_piped(&["--file", other.to_str().unwrap(), "--exit-on-stdin-close"]);

    // Both ends closed, the flagged server's last: once it has gone, the
    // other has had its end of input as long.
    drop(kept.child.stdin.take());
    drop(closed.child.stdin.take());
    let (code, log) = closed
        .exited(EXIT)
        .expect("the flagged server ran on once its stdin closed");
    assert_eq!(code, 0, "{log}");

    // And its syncer has looked at the shutdown flag since: it looks before
    // each flush, and a row reaches the file only by a flush (`--sync 50`).
    let mut c = kept.http();
    c.run("create collection t (name text)");
    c.run(r#"put t {name: "after"}"#);
    let deadline = Instant::now() + EXIT;
    while rows(&path, "t") == 0 {
        assert!(
            Instant::now() < deadline,
            "the syncer never flushed the row"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(kept.child.try_wait().unwrap().is_none(), "the server ended");
    assert_eq!(count(&c.run("get t count")), 1);
    let log = kept.log.lock().unwrap().clone();
    assert!(
        !log.contains("stdin closed") && !log.contains("shutting down"),
        "{log}"
    );
}

/// `/dev/null` ends at once, and so does the server -- cleanly, so a parent
/// that forgot the pipe learns of it at its first start.
#[test]
fn stdin_of_dev_null_ends_the_server_at_once() {
    let path = tmp("stdin", "null.fenec");
    let child = Command::new(env!("CARGO_BIN_EXE_fenec-server"))
        .args(["--http", "127.0.0.1:0", "--file", path.to_str().unwrap()])
        .arg("--exit-on-stdin-close")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("could not start fenec-server");
    let (code, log) = ended(child);
    assert_eq!(code, 0, "{log}");
    assert!(log.contains("stdin closed: shutting down"), "{log}");
    assert!(log.contains("shutting down:"), "{log}");
    // Let go of the file, as a SIGTERM's exit does.
    drop(start(&["--file", path.to_str().unwrap()]));
}

/// A terminal is refused at the start: reading one from the background
/// would stop the whole server (SIGTTIN), its file's lock held.
#[test]
fn a_terminal_on_stdin_is_refused() {
    let (master, slave) = pty();
    let child = Command::new(env!("CARGO_BIN_EXE_fenec-server"))
        .args(["--http", "127.0.0.1:0", "--exit-on-stdin-close"])
        .stdin(Stdio::from(slave))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("could not start fenec-server");
    let (code, log) = ended(child);
    drop(master);
    assert_eq!(code, 2, "{log}");
    assert!(log.contains("stdin is a terminal"), "{log}");
}

/// The exit code and stderr of a child that must end of its own accord,
/// within [`EXIT`]; killed past it.
fn ended(mut child: Child) -> (i32, String) {
    let mut err = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut log = String::new();
        let _ = err.read_to_string(&mut log);
        log
    });
    let deadline = Instant::now() + EXIT;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the server did not end:\n{}", reader.join().unwrap());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    (status.code().unwrap_or(-1), reader.join().unwrap())
}

/// A pseudo-terminal: its master, and its slave to hand a child as stdin.
fn pty() -> (std::fs::File, std::fs::File) {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::FromRawFd;
    // libc's, declared as the server declares `signal`: no dependency.
    extern "C" {
        fn posix_openpt(flags: i32) -> i32;
        fn grantpt(fd: i32) -> i32;
        fn unlockpt(fd: i32) -> i32;
        fn ptsname(fd: i32) -> *const std::ffi::c_char;
    }
    const O_RDWR: i32 = 2;
    #[cfg(target_os = "linux")]
    const O_NOCTTY: i32 = 0o400;
    #[cfg(not(target_os = "linux"))]
    const O_NOCTTY: i32 = 0x20000;
    unsafe {
        let fd = posix_openpt(O_RDWR | O_NOCTTY);
        assert!(fd >= 0, "no pseudo-terminal");
        let master = std::fs::File::from_raw_fd(fd);
        assert_eq!(grantpt(fd), 0);
        assert_eq!(unlockpt(fd), 0);
        let name = std::ffi::CStr::from_ptr(ptsname(fd))
            .to_str()
            .unwrap()
            .to_string();
        let slave = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(O_NOCTTY)
            .open(name)
            .expect("could not open the pseudo-terminal");
        (master, slave)
    }
}
