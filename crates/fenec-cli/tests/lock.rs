//! `<file>.lock` from the shell's side. The shell took none: beside a
//! `fenec-server --file`, its `put` printed "1 records" and exited 0, the
//! server's next writes were appended over it, and what was left of its
//! body behind them made a file no open read again (`corrupt: unknown
//! record kind 120`). Every way the shell writes a file now takes the lock
//! first (`fenec_abi::lock`, as the server, `fenec mcp --file` and the
//! native library do), and is refused, naming the holder, exit 1, while
//! another process has the file open; `--read-only` reads it beside them
//! and takes none.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::{Arc, Mutex};

const BIN: &str = env!("CARGO_BIN_EXE_fenec");

/// A directory of its own, removed at the end.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let d = std::env::temp_dir().join(format!("fenec-cli-lock-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Dir(d)
    }

    fn file(&self, name: &str) -> String {
        self.0.join(name).to_str().unwrap().to_string()
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// `fenec <args>`, its stdin empty.
fn fenec(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// `fenec <args>` with `input` piped in.
fn piped(args: &[&str], input: &str) -> Output {
    let mut child = Command::new(BIN)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // A shell refused at the start may be gone before its input is
    // written: a broken pipe then is the refusal, told by the exit.
    let _ = child.stdin.take().unwrap().write_all(input.as_bytes());
    child.wait_with_output().unwrap()
}

/// The ids of the table a `get ... select id` printed.
fn ids(out: &Output) -> Vec<i64> {
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    text(&out.stdout)
        .lines()
        .skip_while(|l| !l.starts_with("--"))
        .skip(1)
        .take_while(|l| !l.starts_with('('))
        .map(|l| l.trim().parse().unwrap())
        .collect()
}

/// Refused as a held file is: exit 1, the file and its holder named, and
/// nothing written.
fn refused(out: &Output, path: &str, holder: &str, what: &str) {
    let why = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{what}: {why}");
    assert!(
        why.contains(&format!("{path} is open in another process (")),
        "{what}: {why}"
    );
    assert!(why.contains(holder), "{what}: {holder} in {why}");
    assert!(!text(&out.stdout).contains("records"), "{what}: wrote");
}

/// `fenec-server`, built beside the shell by a `cargo test` of the
/// workspace, as CI's, or of both crates. A test of this crate alone may
/// find none: the test says so and passes over the server's half, the
/// rest of this file holding the lock with a process of its own.
fn server_bin() -> Option<PathBuf> {
    let bin =
        Path::new(BIN).with_file_name(format!("fenec-server{}", std::env::consts::EXE_SUFFIX));
    bin.is_file().then_some(bin)
}

/// A `fenec-server --file` on a port the system picks, found by its
/// `listening on:` line, every write fsynced before it is answered (so a
/// read-only open after the answer finds it in the file), and stopped by
/// the end of its stdin: the one shutdown, its lock let go.
struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    addr: String,
    log: Arc<Mutex<String>>,
}

impl Server {
    fn start(bin: &Path, file: &str) -> Server {
        Server::spawn(bin, &["--file", file])
    }

    /// A `--dir` node over `dir`, its admin token `adm`.
    fn node(bin: &Path, dir: &str) -> Server {
        Server::spawn(bin, &["--dir", dir, "--admin-token", "adm"])
    }

    fn spawn(bin: &Path, args: &[&str]) -> Server {
        let mut child = Command::new(bin)
            .args(args)
            .args(["--http", "127.0.0.1:0", "--sync", "always"])
            .arg("--exit-on-stdin-close")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let mut err = BufReader::new(child.stderr.take().unwrap());
        let log = Arc::new(Mutex::new(String::new()));
        let addr = loop {
            let mut line = String::new();
            if err.read_line(&mut line).unwrap_or(0) == 0 {
                panic!(
                    "fenec-server ended before it listened:\n{}",
                    log.lock().unwrap()
                );
            }
            log.lock().unwrap().push_str(&line);
            if let Some((_, rest)) = line.split_once("listening on: http://") {
                break rest.split_whitespace().next().unwrap().to_string();
            }
        };
        let sink = Arc::clone(&log);
        std::thread::spawn(move || {
            let mut line = String::new();
            while err.read_line(&mut line).unwrap_or(0) > 0 {
                sink.lock().unwrap().push_str(&line);
                line.clear();
            }
        });
        Server {
            child,
            stdin,
            addr,
            log,
        }
    }

    /// `POST /query`, the answer's body.
    fn query(&self, q: &str) -> String {
        self.run("/query", q).1
    }

    /// `POST <path>` of a statement: the status and the body.
    fn run(&self, path: &str, q: &str) -> (u16, String) {
        let body = format!(
            "{{\"query\":\"{}\"}}",
            q.replace('\\', "\\\\").replace('"', "\\\"")
        );
        self.ask("POST", path, &body, None)
    }

    /// A request: the status and the body.
    fn ask(&self, method: &str, path: &str, body: &str, token: Option<&str>) -> (u16, String) {
        let mut s = TcpStream::connect(&self.addr).unwrap();
        let auth = token.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
        write!(
            s,
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\n\
             {auth}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            self.addr,
            body.len()
        )
        .unwrap();
        let mut answer = String::new();
        s.read_to_string(&mut answer).unwrap();
        let status = answer
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let body = answer
            .split_once("\r\n\r\n")
            .map_or(answer.clone(), |(_, b)| b.to_string());
        (status, body)
    }

    /// Its stdin ended, the exit code once it is down.
    fn stop(mut self) -> i32 {
        drop(self.stdin.take());
        let status = self.child.wait().unwrap();
        assert!(
            status.success(),
            "fenec-server: {status}\n{}",
            self.log.lock().unwrap()
        );
        status.code().unwrap()
    }
}

/// The reported write, against a real server. Taken by nothing, the
/// shell's write -- longer than the server's two after it -- landed past
/// the server's end, the server's two landed over its first bytes, and the
/// rest of it, left behind them, was read as a record: the file opened no
/// more. Now the shell is refused while the server holds the file, reads it
/// `--read-only` beside it, and writes once the server is gone, the file
/// whole.
#[test]
fn a_shell_beside_a_server_is_refused_and_the_file_stays_whole() {
    let Some(bin) = server_bin() else {
        eprintln!(
            "passed over: no fenec-server beside {BIN} (cargo test -p fenec-cli -p fenec-server)"
        );
        return;
    };
    let d = Dir::new("server");
    let p = d.file("x.fenec");
    let server = Server::start(&bin, &p);
    assert!(server
        .query("create collection notes (body text)")
        .contains("created"));
    assert!(server
        .query(r#"put notes {id: 1, body: "server one"}"#)
        .contains("\"affected\":1"));
    let pid = format!("pid {})", server.child.id());

    let put = format!(r#"put notes {{id: 2, body: "{}"}}"#, "x".repeat(300));
    let out = fenec(&[&p, "-c", &put]);
    refused(&out, &p, &pid, "-c");
    // Told what to do by what holds it: a server is asked over HTTP.
    let why = text(&out.stderr);
    assert!(why.contains("(fenec-server, pid "), "{why}");
    assert!(why.contains("query it over HTTP"), "{why}");
    assert!(why.contains(&format!("fenec --read-only {p}")), "{why}");
    refused(&piped(&[&p], &format!("{put}\n")), &p, &pid, "piped");
    refused(&fenec(&[&p, "-c", "get notes"]), &p, &pid, "a read");

    // Read-only, beside it: the file as it stands, and no write.
    assert_eq!(
        ids(&fenec(&["--read-only", &p, "-c", "get notes select id"])),
        [1]
    );
    let out = fenec(&["--read-only", &p, "-c", &put]);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("--read-only"));

    for id in [3, 4] {
        let a = server.query(&format!(r#"put notes {{id: {id}, body: "server"}}"#));
        assert!(a.contains("\"affected\":1"), "{a}");
    }
    assert_eq!(server.stop(), 0);

    // Let go with the server: the shell writes, and the file opens whole.
    let out = fenec(&[&p, "-c", &put]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("1 records"));
    assert_eq!(
        ids(&fenec(&[&p, "-c", "get notes select id order id"])),
        [1, 2, 3, 4]
    );
    // And a server opens it again.
    let again = Server::start(&bin, &p);
    assert!(again.query("get notes count").contains('4'));
    assert_eq!(again.stop(), 0);
}

/// A file served through a symbolic link, as #420's reviewer found it: a
/// compact and a checkpoint renamed their image over the link, which was a
/// file of its own from then on, so a shell over the link took that file's
/// lock beside the server, wrote, and the file ended `corrupt: unknown
/// record kind 120`. Each renames over the file the link leads to now: the
/// link stays a link, the shell is refused through it and by the file's
/// own name after a compact, and a checkpoint at the server's stop and the
/// shell's `.checkpoint` keep it so, every row in the one file.
#[test]
#[cfg(unix)]
fn a_file_served_through_a_link_stays_one_file_across_a_compact() {
    let Some(bin) = server_bin() else {
        eprintln!("passed over: no fenec-server beside {BIN}");
        return;
    };
    let d = Dir::new("served-link");
    let (real, link) = (d.file("real.fenec"), d.file("link.fenec"));
    std::os::unix::fs::symlink("real.fenec", &link).unwrap();
    let is_link = || {
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    };
    let server = Server::start(&bin, &link);
    for q in [
        "create collection notes (body text, e vector<2> @hnsw)",
        r#"put notes {id: 1, body: "one", e: [1, 0]}"#,
        r#"put notes {id: 2, body: "two", e: [0, 1]}"#,
        "del notes where id = 1",
        "compact",
    ] {
        let (status, body) = server.run("/query", q);
        assert_eq!(status, 200, "{q}: {body}");
    }
    assert!(is_link(), "the compact renamed over the link");
    let pid = format!("(fenec-server, pid {})", server.child.id());
    let put = r#"put notes {id: 9, body: "shell", e: [1, 1]}"#;
    refused(&fenec(&[&link, "-c", put]), &link, &pid, "through the link");
    refused(
        &fenec(&[&real, "-c", put]),
        &real,
        &pid,
        "by the file's name",
    );
    let (status, body) = server.run("/query", r#"put notes {id: 3, body: "three", e: [1, 1]}"#);
    assert_eq!(status, 200, "{body}");
    // Its stop checkpoints the graph: another rewrite.
    assert_eq!(server.stop(), 0);
    assert!(is_link(), "the checkpoint renamed over the link");
    let out = piped(&[&link], ".checkpoint\n");
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(is_link(), "the shell's checkpoint renamed over the link");
    for p in [&link, &real] {
        assert_eq!(
            ids(&fenec(&[p, "-c", "get notes select id order id"])),
            [2, 3]
        );
    }
}

/// A `--dir` node's tenant file, as #414's reviewer found it: the node took
/// no lock, so `fenec T/acme.fenec -c 'put ...'` beside it wrote, and the
/// node's next writes left the file unreadable (`corrupt: unknown record
/// kind 120`). Each open tenant holds its file's lock now, released with its
/// database: the shell is refused while the node has the tenant open --
/// through a link to the file too -- and the node is refused the tenant, an
/// open and a delete, while a shell has it; each goes on once the other let
/// go, the file whole.
#[test]
#[cfg(unix)]
fn a_tenant_s_file_is_held_by_its_node_and_refused_to_it() {
    let Some(bin) = server_bin() else {
        eprintln!("passed over: no fenec-server beside {BIN}");
        return;
    };
    let d = Dir::new("node");
    let tenants = d.file("T");
    std::fs::create_dir_all(&tenants).unwrap();
    let p = format!("{tenants}/acme.fenec");
    let node = Server::node(&bin, &tenants);
    let (status, body) = node.ask("PUT", "/_admin/tenants/acme", "", Some("adm"));
    assert_eq!(status, 201, "{body}");
    let (status, body) = node.run("/t/acme/query", "create collection notes (body text)");
    assert_eq!(status, 200, "{body}");
    let (status, body) = node.run("/t/acme/query", r#"put notes {id: 1, body: "node"}"#);
    assert_eq!(status, 200, "{body}");

    let pid = format!("(fenec-server, pid {})", node.child.id());
    let put = format!(r#"put notes {{id: 2, body: "{}"}}"#, "x".repeat(300));
    refused(&fenec(&[&p, "-c", &put]), &p, &pid, "-c");
    let link = d.file("acme-link.fenec");
    std::os::unix::fs::symlink(&p, &link).unwrap();
    refused(&fenec(&[&link, "-c", &put]), &link, &pid, "through a link");
    for id in [3, 4] {
        let (status, body) = node.run("/t/acme/query", &format!("put notes {{id: {id}}}"));
        assert_eq!(status, 200, "{body}");
    }
    assert_eq!(node.stop(), 0);

    // The other way round: a shell has the file, and a node started over
    // the directory is refused the tenant -- named -- but serves the rest.
    let mut shell = Command::new(BIN)
        .arg(&p)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = shell.stdin.take().unwrap();
    let mut stdout = BufReader::new(shell.stdout.take().unwrap());
    writeln!(stdin, ".tables").unwrap();
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "notes");
    let node = Server::node(&bin, &tenants);
    let held = format!("(fenec, pid {})", shell.id());
    let (status, body) = node.run("/t/acme/query", "get notes");
    assert_eq!(status, 500, "{body}");
    assert!(
        body.contains("open in another process") && body.contains(&held),
        "{body}"
    );
    let (status, body) = node.ask("DELETE", "/_admin/tenants/acme", "", Some("adm"));
    assert_eq!(status, 500, "{body}");
    assert!(body.contains(&held), "{body}");
    assert!(Path::new(&p).exists(), "deleted from under the shell");
    let (status, body) = node.ask("PUT", "/_admin/tenants/beta", "", Some("adm"));
    assert_eq!(status, 201, "{body}");

    // The shell writes and quits: the node opens the tenant, every row in.
    writeln!(stdin, "{put};").unwrap();
    drop(stdin);
    let out = shell.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let (status, body) = node.run("/t/acme/query", "get notes select id order id");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"[{"id":1},{"id":2},{"id":3},{"id":4}]"#);
    assert_eq!(node.stop(), 0);
    assert_eq!(
        ids(&fenec(&[&p, "-c", "get notes select id order id"])),
        [1, 2, 3, 4]
    );
}

/// Two shells over one file: the second is refused while the first has it
/// open, whichever way it is started, and opens it once the first quit.
#[test]
fn a_second_shell_is_refused_until_the_first_has_quit() {
    let d = Dir::new("two");
    let p = d.file("two.fenec");
    let mut first = Command::new(BIN)
        .arg(&p)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = first.stdin.take().unwrap();
    let mut stdout = BufReader::new(first.stdout.take().unwrap());
    // A `.` command with nothing waiting runs at once: answered, the file
    // is open, and held.
    writeln!(stdin, ".tables").unwrap();
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "(no collections)");
    let holder = format!("(fenec, pid {})", first.id());

    let put = r#"put notes {body: "second"}"#;
    let out = fenec(&[&p, "-c", put]);
    refused(&out, &p, &holder, "-c");
    // A session is quit, not queried over HTTP.
    let why = text(&out.stderr);
    assert!(
        why.contains("quit that fenec session first") && !why.contains("HTTP"),
        "{why}"
    );
    refused(&piped(&[&p], &format!("{put}\n")), &p, &holder, "piped");
    let out = fenec(&["--read-only", &p, "-c", "collections"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));

    // `.save` over the file the session has open is refused too: written
    // in place, it cut short the file the session maps. Piped, both run at
    // the end of the input, the `.save` behind the statement.
    writeln!(stdin, "create collection notes (body text)\n.save {p}").unwrap();
    drop(stdin);
    let out = first.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let why = text(&out.stderr);
    assert!(
        why.contains(&format!("{p} is open in this process")),
        "{why}"
    );

    let out = fenec(&[&p, "-c", put]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("1 records"));
}

/// Every way the program writes a database file meets the lock: the shell
/// with `-c`, piped or with nothing to read, a read too (the lock is taken
/// at the open), `fenec import`, `backup` and `restore` into it, `fenec mcp
/// --file`, and `.save` over it. The holder here is this test's process,
/// which another process's refusal names by its id. Let go, each opens it.
#[test]
fn every_way_the_program_writes_a_held_file_is_refused() {
    let d = Dir::new("ways");
    let p = d.file("held.fenec");
    let made = fenec(&[&p, "-c", "create collection notes (body text)"]);
    assert!(made.status.success(), "{}", text(&made.stderr));
    let held = fenec_abi::lock::hold(&p).unwrap();
    let me = format!("pid {})", std::process::id());
    let put = r#"put notes {body: "a"}"#;

    refused(&fenec(&[&p, "-c", put]), &p, &me, "-c");
    refused(&fenec(&[&p, "-c", "get notes"]), &p, &me, "a read");
    refused(&piped(&[&p], &format!("{put}\n")), &p, &me, "piped");
    refused(&fenec(&[&p]), &p, &me, "nothing to read");
    if cfg!(feature = "import") {
        let source = d.file("none.sqlite");
        let out = fenec(&["import", &source, "--table", "t", "--file", &p]);
        refused(&out, &p, &me, "import");
    }
    if cfg!(feature = "backup") {
        let archive = d.file("no-archive");
        refused(&fenec(&["restore", &archive, &p]), &p, &me, "restore");
        let out = fenec(&["backup", "http://127.0.0.1:9", &p, "--token", "t"]);
        refused(&out, &p, &me, "backup");
    }
    if cfg!(feature = "mcp") {
        refused(&fenec(&["mcp", "--file", &p]), &p, &me, "mcp");
    }
    // `.save` from a session in memory: told, and the file left as it is.
    let out = piped(
        &[],
        &format!("create collection other (x int)\n.save {p}\n"),
    );
    let why = text(&out.stderr);
    assert!(why.contains(&format!("{p} is open in another process (")) && why.contains(&me));
    // Read-only takes no lock, and reads it.
    let out = fenec(&["--read-only", &p, "-c", "collections"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("notes") && !text(&out.stdout).contains("other"));
    // ... and writes nothing to it, a `.checkpoint` and a `.save` over it
    // neither; a copy elsewhere is a file of its own.
    let copy = d.file("copy.fenec");
    let out = piped(
        &["--read-only", &p],
        &format!(".checkpoint\n.save {p}\n.save {copy}\n"),
    );
    let why = text(&out.stderr);
    assert_eq!(why.matches("opened --read-only").count(), 2, "{why}");
    assert!(
        text(&out.stdout).contains(&format!("{copy} written")),
        "{why}"
    );
    let out = fenec(&[&copy, "-c", "collections"]);
    assert!(text(&out.stdout).contains("notes"), "{}", text(&out.stderr));

    drop(held);
    let out = fenec(&[&p, "-c", put]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    // `.save` writes a file that opens as the session's database.
    let q = d.file("saved.fenec");
    let out = piped(
        &[],
        &format!("create collection other (x int)\n.save {q}\n"),
    );
    assert!(
        text(&out.stdout).contains("written"),
        "{}",
        text(&out.stderr)
    );
    let out = fenec(&[&q, "-c", "collections"]);
    assert!(text(&out.stdout).contains("other"), "{}", text(&out.stderr));
}

/// A file in a directory this user may only read: the lock file cannot be
/// made, so the shell says so and names `--read-only`, which reads it
/// without one, as a read there did before the lock. Run as root, the
/// directory refuses nothing and there is nothing to show.
#[cfg(unix)]
#[test]
fn a_file_whose_lock_cannot_be_made_is_read_with_read_only() {
    use std::os::unix::fs::PermissionsExt;
    let d = Dir::new("readonly-dir");
    let inner = d.0.join("ro");
    std::fs::create_dir_all(&inner).unwrap();
    let p = inner.join("ro.fenec").to_str().unwrap().to_string();
    let made = fenec(&[&p, "-c", "create collection notes (body text)"]);
    assert!(made.status.success(), "{}", text(&made.stderr));
    std::fs::remove_file(format!("{p}.lock")).unwrap();
    let mode = |m| std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(m)).unwrap();
    mode(0o555);
    let out = fenec(&[&p, "-c", "collections"]);
    let ro = fenec(&["--read-only", &p, "-c", "collections"]);
    mode(0o755);
    if out.status.success() {
        eprintln!("passed over: the directory refused nothing (run as root?)");
        return;
    }
    let why = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{why}");
    assert!(why.contains(&format!("could not take {p}.lock")), "{why}");
    assert!(
        why.contains(&format!("`fenec --read-only {p}` reads it without a lock")),
        "{why}"
    );
    assert_eq!(ro.status.code(), Some(0), "{}", text(&ro.stderr));
    assert!(text(&ro.stdout).contains("notes"));
}

/// `fenec restore` renamed its file over whatever had the name, a
/// database written since included, and exited 0; a backup into a file did
/// the same. Each refuses a file there already now (exit 2, the file left
/// as it was) unless `--force` says to replace it.
#[cfg(feature = "backup")]
#[test]
fn a_restore_or_a_backup_never_writes_over_a_database_unasked() {
    let d = Dir::new("restore");
    let archive = d.file("archive");
    let mut db = fenec_core::prelude::Database::new();
    for q in [
        "create collection notes (body text)",
        r#"put notes {body: "archived"}"#,
    ] {
        db.execute(&fenec_ql::parse_one(q).unwrap()).unwrap();
    }
    fenec_http::archive::Archive::with_key(&archive, None)
        .unwrap()
        .add_image(db.change_seq(), &db.snapshot())
        .unwrap();
    let out = d.file("out.fenec");
    let restored = fenec(&["restore", &archive, &out]);
    assert_eq!(
        restored.status.code(),
        Some(0),
        "{}",
        text(&restored.stderr)
    );
    let wrote = fenec(&[&out, "-c", r#"put notes {body: "written since"}"#]);
    assert!(wrote.status.success(), "{}", text(&wrote.stderr));
    let before = std::fs::read(&out).unwrap();

    let again = fenec(&["restore", &archive, &out]);
    let why = text(&again.stderr);
    assert_eq!(again.status.code(), Some(2), "{why}");
    assert!(
        why.contains("is there already") && why.contains("--force"),
        "{why}"
    );
    assert_eq!(std::fs::read(&out).unwrap(), before, "the file was written");
    // A backup into the name: refused before the primary is asked, which
    // here is no one.
    let backup = fenec(&["backup", "http://127.0.0.1:9", &out, "--token", "t"]);
    let why = text(&backup.stderr);
    assert_eq!(backup.status.code(), Some(2), "{why}");
    assert!(why.contains("is there already"), "{why}");
    assert_eq!(std::fs::read(&out).unwrap(), before);

    // Forced, the restore replaces it: the archive's row alone.
    let forced = fenec(&["restore", &archive, &out, "--force"]);
    assert_eq!(forced.status.code(), Some(0), "{}", text(&forced.stderr));
    let rows = text(&fenec(&[&out, "-c", "get notes select body"]).stdout);
    assert!(
        rows.contains("archived") && !rows.contains("written since"),
        "{rows}"
    );
    // `--force` belongs to those two alone.
    let verify = fenec(&["verify", &archive, "--force"]);
    assert_eq!(verify.status.code(), Some(2), "{}", text(&verify.stderr));
}
