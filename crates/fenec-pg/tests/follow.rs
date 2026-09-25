//! `fenec-pg --follow` against a live PostgreSQL (`make import-test`, which
//! starts one in Docker): a table's copy and its changes served over the pg
//! wire and HTTP as they commit, a client's write to the mirror refused, and
//! a server stopped -- by a signal, or killed outright -- started again over
//! its file without losing a row.
//!
//!     cargo test -p fenec-pg --test follow -- --ignored

#![cfg(unix)]

use fenec_pg::client::{Client, Url};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const URL: &str = "postgres://postgres:fenec@127.0.0.1:55432/fenecbench";

fn source_text() -> String {
    std::env::var("FENEC_TEST_PG_URL").unwrap_or_else(|_| URL.into())
}

fn source() -> Url {
    Url::parse(&source_text()).unwrap()
}

fn pg() -> Client {
    Client::connect(&source()).expect("no PostgreSQL to follow (make pgvector-up)")
}

/// A table of `rows` rows, with no slot or publication left from before:
/// the follower names both `fenec_<collection>`.
fn table(c: &mut Client, t: &str, rows: u64) {
    for q in [
        format!("select pg_drop_replication_slot('fenec_{t}') from pg_replication_slots where slot_name = 'fenec_{t}'"),
        format!("drop publication if exists fenec_{t}"),
        format!("drop table if exists {t}"),
        format!("create table {t} (id bigint primary key, title text)"),
        format!("insert into {t} select g, 'row ' || g from generate_series(1, {rows}) g"),
    ] {
        c.query(&q).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
}

fn forget(c: &mut Client, t: &str) {
    let _ = c.query(&format!("select pg_drop_replication_slot('fenec_{t}')"));
    let _ = c.query(&format!("drop publication if exists fenec_{t}"));
    let _ = c.query(&format!("drop table if exists {t}"));
}

struct Server {
    child: Child,
    pg: u16,
    http: u16,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    fn client(&self) -> Client {
        Client::connect(&Url {
            user: "fenec".into(),
            password: None,
            host: "127.0.0.1".into(),
            port: self.pg,
            database: "fenec".into(),
        })
        .unwrap()
    }

    fn http(&self, method: &str, path: &str, body: &str) -> (u16, String) {
        let mut s = TcpStream::connect(("127.0.0.1", self.http)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        write!(
            s,
            "{method} {path} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        let body = out
            .split_once("\r\n\r\n")
            .map_or("", |(_, b)| b)
            .to_string();
        (out[9..12].parse().unwrap(), body)
    }

    fn count(&self, t: &str) -> u64 {
        let rows = self.client().query(&format!("get {t} count")).unwrap().rows;
        rows[0][0].as_deref().unwrap().parse().unwrap()
    }

    /// Waits until a row of the mirror has `title`.
    fn shows(&self, t: &str, title: &str) {
        let q = format!("get {t} where title = \"{title}\" count");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let rows = self.client().query(&q).unwrap().rows;
            if rows[0][0].as_deref() == Some("1") {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "no row of {t} has title `{title}`"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Waits until the mirror holds `n` rows.
    fn holds(&self, t: &str, n: u64) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.count(t) != n {
            assert!(
                Instant::now() < deadline,
                "the mirror of {t} stayed at {}",
                self.count(t)
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Asks it to stop, as a supervisor does, and waits until it has.
    fn stop(mut self) {
        let _ = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status();
        let _ = self.child.wait();
    }
}

fn tmp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fenecpg-follow-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("mirror.fenec")
}

/// Starts the server over `file`, following `t`, and waits until it streams.
fn start(file: &Path, t: &str) -> Server {
    let mut child = Command::new(env!("CARGO_BIN_EXE_fenec-pg"))
        .args(["--listen", "127.0.0.1:0", "--http", "127.0.0.1:0"])
        .arg("--file")
        .arg(file)
        .args(["--follow", &source_text(), "--follow-table", t])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("could not start fenec-pg");
    let mut err = BufReader::new(child.stderr.take().unwrap());
    let port = |line: &str, after: &str| -> Option<u16> {
        line.split(after)
            .nth(1)?
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .ok()
    };
    let (mut pg, mut http, mut streaming) = (None, None, false);
    let mut seen = String::new();
    while pg.is_none() || http.is_none() || !streaming {
        let mut line = String::new();
        if err.read_line(&mut line).unwrap_or(0) == 0 {
            panic!("fenec-pg ended before it streamed:\n{seen}");
        }
        seen.push_str(&line);
        if line.contains("postgres://localhost:") {
            pg = port(&line, "localhost:");
        } else if line.contains("listening on: http://127.0.0.1:") {
            http = port(&line, "127.0.0.1:");
        } else if line.contains("streaming its changes") {
            streaming = true;
        }
    }
    std::thread::spawn(move || {
        let mut line = String::new();
        while err.read_line(&mut line).unwrap_or(0) > 0 {
            line.clear();
        }
    });
    Server {
        child,
        pg: pg.unwrap(),
        http: http.unwrap(),
    }
}

#[test]
#[ignore = "needs a PostgreSQL to follow: make import-test"]
fn a_followed_table_is_served_and_takes_no_other_write() {
    let t = "fenec_pg_follow_served";
    let mut c = pg();
    table(&mut c, t, 50);
    let file = tmp("served");
    let s = start(&file, t);
    assert_eq!(s.count(t), 50);

    // A change committed there is served here, over either wire: each in
    // its own transaction, and applied in their order.
    c.query(&format!("insert into {t} values (51, 'new')"))
        .unwrap();
    c.query(&format!("update {t} set title = 'changed' where id = 7"))
        .unwrap();
    c.query(&format!("delete from {t} where id = 8")).unwrap();
    c.query(&format!("insert into {t} values (52, 'last')"))
        .unwrap();
    s.shows(t, "last");
    assert_eq!(s.count(t), 51);
    let (status, body) = s.http(
        "POST",
        "/query",
        &format!(r#"{{"query":"get {t} where id = 7"}}"#),
    );
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("changed"), "{body}");

    // The mirror is the follower's to write; the rest of the file is not.
    let err = s
        .client()
        .query(&format!("put {t} {{id: 99, title: \"mine\"}}"))
        .unwrap_err();
    assert!(
        err.to_string().contains("mirrors a PostgreSQL table"),
        "{err}"
    );
    let (status, _) = s.http(
        "POST",
        "/query",
        &format!(r#"{{"query":"del {t} where id = 1"}}"#),
    );
    assert_eq!(status, 403);
    s.client().query("create collection notes (n int)").unwrap();
    s.client().query("put notes {n: 1}").unwrap();
    assert_eq!(s.count(t), 51);

    drop(s);
    forget(&mut c, t);
}

#[test]
#[ignore = "needs a PostgreSQL to follow: make import-test"]
fn a_server_stopped_or_killed_goes_on_without_losing_a_row() {
    let t = "fenec_pg_follow_restart";
    let mut c = pg();
    table(&mut c, t, 20);
    let file = tmp("restart");

    // Stopped by a signal: its last changes on disk and confirmed, and the
    // ones committed while it was down taken in when it is back.
    let s = start(&file, t);
    s.holds(t, 20);
    s.stop();
    c.query(&format!(
        "insert into {t} select g, 'down ' || g from generate_series(21, 40) g"
    ))
    .unwrap();
    let s = start(&file, t);
    s.holds(t, 40);

    // Killed outright while the table is written to: what it applied and
    // did not confirm comes again, and changes nothing.
    let written = Arc::new(AtomicU64::new(40));
    let done = Arc::new(AtomicBool::new(false));
    let writer = {
        let (written, done) = (Arc::clone(&written), Arc::clone(&done));
        std::thread::spawn(move || {
            let mut c = pg();
            while !done.load(Ordering::Relaxed) {
                let id = written.load(Ordering::Relaxed) + 1;
                c.query(&format!("insert into {t} values ({id}, 'burst')"))
                    .unwrap();
                written.store(id, Ordering::Relaxed);
            }
        })
    };
    std::thread::sleep(Duration::from_millis(300));
    drop(s);
    std::thread::sleep(Duration::from_millis(300));
    done.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    let s = start(&file, t);
    s.holds(t, written.load(Ordering::Relaxed));

    drop(s);
    forget(&mut c, t);
}
