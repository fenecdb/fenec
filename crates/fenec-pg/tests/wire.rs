//! `fenec-pg` wire protocol tests.
//!
//! The server is brought up in-process on `127.0.0.1:0`; the client side is
//! hand-coded here, so the assertions are on the real byte stream (not on
//! the leniency of a client library).

use fenec_core::engine::Database;
use fenec_core::value::Value;
use fenec_pg::crypto::*;
use fenec_pg::server::{Auth, SyncPolicy};
use fenec_pg::{Config, PgPlugin, Server};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

// ------------------------------------------------------------ test server

#[allow(dead_code)]
struct Harness {
    port: u16,
    db: Arc<RwLock<Database>>,
}

fn start(mut cfg: Config, db: Database) -> Harness {
    cfg.addr = "127.0.0.1:0".into();
    let db = Arc::new(RwLock::new(db));
    let server = Server::new(Arc::clone(&db), cfg);
    let listener = server.bind().expect("bind");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    Harness { port, db }
}

fn trust_server() -> Harness {
    let mut db = Database::new();
    db.install_plugin(&PgPlugin).unwrap();
    start(Config::default(), db)
}

// -------------------------------------------------------------- client

/// A single message from the server.
#[derive(Debug, Clone)]
struct Msg {
    tag: u8,
    body: Vec<u8>,
}

impl Msg {
    /// The SQLSTATE code inside an ErrorResponse.
    fn sqlstate(&self) -> Option<String> {
        self.field(b'C')
    }
    fn message(&self) -> Option<String> {
        self.field(b'M')
    }
    fn field(&self, key: u8) -> Option<String> {
        let mut pos = 0;
        while pos < self.body.len() && self.body[pos] != 0 {
            let k = self.body[pos];
            pos += 1;
            let start = pos;
            while pos < self.body.len() && self.body[pos] != 0 {
                pos += 1;
            }
            if k == key {
                return Some(String::from_utf8_lossy(&self.body[start..pos]).into_owned());
            }
            pos += 1;
        }
        None
    }
    /// RowDescription -> (name, type oid)
    fn columns(&self) -> Vec<(String, i32)> {
        let n = i16::from_be_bytes([self.body[0], self.body[1]]) as usize;
        let mut pos = 2;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let start = pos;
            while self.body[pos] != 0 {
                pos += 1;
            }
            let name = String::from_utf8_lossy(&self.body[start..pos]).into_owned();
            pos += 1;
            let oid = i32::from_be_bytes([
                self.body[pos + 6],
                self.body[pos + 7],
                self.body[pos + 8],
                self.body[pos + 9],
            ]);
            pos += 18;
            out.push((name, oid));
        }
        out
    }
    /// ParameterDescription -> list of oids
    fn param_oids(&self) -> Vec<i32> {
        let n = i16::from_be_bytes([self.body[0], self.body[1]]) as usize;
        (0..n)
            .map(|i| {
                let p = 2 + i * 4;
                i32::from_be_bytes([
                    self.body[p],
                    self.body[p + 1],
                    self.body[p + 2],
                    self.body[p + 3],
                ])
            })
            .collect()
    }
    /// DataRow -> cells
    fn cells(&self) -> Vec<Option<String>> {
        let n = i16::from_be_bytes([self.body[0], self.body[1]]) as usize;
        let mut pos = 2;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let len = i32::from_be_bytes([
                self.body[pos],
                self.body[pos + 1],
                self.body[pos + 2],
                self.body[pos + 3],
            ]);
            pos += 4;
            if len < 0 {
                out.push(None);
            } else {
                let end = pos + len as usize;
                out.push(Some(
                    String::from_utf8_lossy(&self.body[pos..end]).into_owned(),
                ));
                pos = end;
            }
        }
        out
    }
    fn tag_text(&self) -> String {
        String::from_utf8_lossy(&self.body[..self.body.len().saturating_sub(1)]).into_owned()
    }
}

#[derive(Debug)]
struct Client {
    s: TcpStream,
    pid: i32,
    secret: i32,
}

fn cstr(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(s.as_bytes());
    out.push(0);
}

fn framed(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut v = vec![tag];
    v.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    v.extend_from_slice(body);
    v
}

impl Client {
    fn connect(port: u16, user: &str, password: Option<&str>) -> Result<Client, String> {
        let s = TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.to_string())?;
        s.set_read_timeout(Some(Duration::from_secs(20))).ok();
        let mut c = Client {
            s,
            pid: 0,
            secret: 0,
        };
        let mut body = 196_608i32.to_be_bytes().to_vec();
        cstr(&mut body, "user");
        cstr(&mut body, user);
        body.push(0);
        let mut pkt = ((body.len() + 4) as i32).to_be_bytes().to_vec();
        pkt.extend_from_slice(&body);
        c.s.write_all(&pkt).map_err(|e| e.to_string())?;

        // The authentication flow
        loop {
            let m = c.read_msg().map_err(|e| e.to_string())?;
            match m.tag {
                b'R' => {
                    let code = i32::from_be_bytes([m.body[0], m.body[1], m.body[2], m.body[3]]);
                    match code {
                        0 => {}
                        3 => {
                            let mut b = Vec::new();
                            cstr(&mut b, password.unwrap_or(""));
                            c.s.write_all(&framed(b'p', &b))
                                .map_err(|e| e.to_string())?;
                        }
                        10 => c.scram(password.ok_or("a password is required")?)?,
                        11 | 12 => {}
                        other => return Err(format!("unexpected auth code {other}")),
                    }
                }
                b'K' => {
                    c.pid = i32::from_be_bytes([m.body[0], m.body[1], m.body[2], m.body[3]]);
                    c.secret = i32::from_be_bytes([m.body[4], m.body[5], m.body[6], m.body[7]]);
                }
                b'E' => {
                    return Err(format!(
                        "{} {}",
                        m.sqlstate().unwrap_or_default(),
                        m.message().unwrap_or_default()
                    ))
                }
                b'Z' => return Ok(c),
                _ => {}
            }
        }
    }

    /// The SCRAM-SHA-256 client side (RFC 5802). The server's unit tests
    /// verify the flow on their own; the point here is the *wire framing*.
    fn scram(&mut self, password: &str) -> Result<(), String> {
        let cnonce = "abcdefghijklmnopqr";
        let client_first_bare = format!("n=,r={cnonce}");
        let mut b = Vec::new();
        cstr(&mut b, "SCRAM-SHA-256");
        let initial = format!("n,,{client_first_bare}");
        b.extend_from_slice(&(initial.len() as i32).to_be_bytes());
        b.extend_from_slice(initial.as_bytes());
        self.s
            .write_all(&framed(b'p', &b))
            .map_err(|e| e.to_string())?;

        let m = self.read_msg().map_err(|e| e.to_string())?;
        if m.tag == b'E' {
            return Err(m.message().unwrap_or_default());
        }
        let server_first = String::from_utf8_lossy(&m.body[4..]).into_owned();
        let mut nonce = String::new();
        let mut salt = Vec::new();
        let mut iters = 0u32;
        for kv in server_first.split(',') {
            match kv.split_at(2) {
                ("r=", v) => nonce = v.to_string(),
                ("s=", v) => salt = b64_decode(v).ok_or("could not decode the salt")?,
                ("i=", v) => iters = v.parse().map_err(|_| "could not parse i")?,
                _ => {}
            }
        }
        let without_proof = format!("c=biws,r={nonce}");
        let auth = format!("{client_first_bare},{server_first},{without_proof}");
        let salted = pbkdf2_sha256(password.as_bytes(), &salt, iters);
        let client_key = hmac_sha256(&salted, b"Client Key");
        let sig = hmac_sha256(&sha256(&client_key), auth.as_bytes());
        let proof: Vec<u8> = client_key.iter().zip(sig).map(|(a, b)| a ^ b).collect();
        let final_msg = format!("{without_proof},p={}", b64_encode(&proof));
        self.s
            .write_all(&framed(b'p', final_msg.as_bytes()))
            .map_err(|e| e.to_string())?;

        // AuthenticationSASLFinal: the server signature is verified.
        let m = self.read_msg().map_err(|e| e.to_string())?;
        if m.tag == b'E' {
            return Err(m.message().unwrap_or_default());
        }
        let v = String::from_utf8_lossy(&m.body[4..]).into_owned();
        let server_key = hmac_sha256(&salted, b"Server Key");
        let expected = format!(
            "v={}",
            b64_encode(&hmac_sha256(&server_key, auth.as_bytes()))
        );
        if v != expected {
            return Err("the server signature could not be verified".into());
        }
        Ok(())
    }

    fn read_msg(&mut self) -> std::io::Result<Msg> {
        let mut head = [0u8; 5];
        self.s.read_exact(&mut head)?;
        let len = i32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
        let mut body = vec![0u8; len - 4];
        self.s.read_exact(&mut body)?;
        Ok(Msg { tag: head[0], body })
    }

    /// Reads until ReadyForQuery arrives.
    fn until_ready(&mut self) -> Vec<Msg> {
        let mut out = Vec::new();
        loop {
            let m = self.read_msg().expect("could not read a message");
            let done = m.tag == b'Z';
            out.push(m);
            if done {
                return out;
            }
        }
    }

    fn simple(&mut self, sql: &str) -> Vec<Msg> {
        let mut b = Vec::new();
        cstr(&mut b, sql);
        self.s.write_all(&framed(b'Q', &b)).unwrap();
        self.until_ready()
    }

    /// Parse / (Describe) / Bind / Execute / Sync
    fn extended(&mut self, sql: &str, params: &[&str], describe: bool) -> Vec<Msg> {
        let mut out = Vec::new();
        let mut b = Vec::new();
        cstr(&mut b, "st");
        cstr(&mut b, sql);
        b.extend_from_slice(&0i16.to_be_bytes());
        out.extend_from_slice(&framed(b'P', &b));

        if describe {
            let mut d = vec![b'S'];
            cstr(&mut d, "st");
            out.extend_from_slice(&framed(b'D', &d));
        }

        let mut bind = Vec::new();
        cstr(&mut bind, "po");
        cstr(&mut bind, "st");
        bind.extend_from_slice(&0i16.to_be_bytes());
        bind.extend_from_slice(&(params.len() as i16).to_be_bytes());
        for p in params {
            bind.extend_from_slice(&(p.len() as i32).to_be_bytes());
            bind.extend_from_slice(p.as_bytes());
        }
        bind.extend_from_slice(&0i16.to_be_bytes());
        out.extend_from_slice(&framed(b'B', &bind));

        let mut e = Vec::new();
        cstr(&mut e, "po");
        e.extend_from_slice(&0i32.to_be_bytes());
        out.extend_from_slice(&framed(b'E', &e));
        out.extend_from_slice(&framed(b'S', &[]));
        self.s.write_all(&out).unwrap();
        self.until_ready()
    }

    /// Sends the query without waiting for a response.
    fn send(&mut self, sql: &str) {
        let mut b = Vec::new();
        cstr(&mut b, sql);
        self.s.write_all(&framed(b'Q', &b)).unwrap();
    }
}

/// Sends a cancel request from a separate connection.
fn send_cancel(port: u16, pid: i32, secret: i32) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let mut pkt = 16i32.to_be_bytes().to_vec();
    pkt.extend_from_slice(&80_877_102i32.to_be_bytes());
    pkt.extend_from_slice(&pid.to_be_bytes());
    pkt.extend_from_slice(&secret.to_be_bytes());
    s.write_all(&pkt).unwrap();
}

fn tags(msgs: &[Msg]) -> Vec<char> {
    msgs.iter()
        .filter(|m| m.tag != b'Z')
        .map(|m| m.tag as char)
        .collect()
}

fn find(msgs: &[Msg], tag: u8) -> Option<&Msg> {
    msgs.iter().find(|m| m.tag == tag)
}

// ---------------------------------------------------------------- tests

#[test]
fn simple_query_roundtrip() {
    let h = trust_server();
    let mut c = Client::connect(h.port, "fenec", None).unwrap();

    let r = c.simple("SELECT version()");
    assert!(find(&r, b'T').unwrap().columns()[0].0 == "version");
    assert!(find(&r, b'D').unwrap().cells()[0]
        .as_deref()
        .unwrap()
        .contains("fenecdb"));

    c.simple("create collection t (name text, year int @hash, e vector<3> @hnsw(cosine))");
    assert_eq!(
        find(
            &c.simple("put t {name: \"a\", year: 2024, e: [1,0,0]}"),
            b'C'
        )
        .unwrap()
        .tag_text(),
        "INSERT 0 1"
    );

    let r = c.simple("get t select name, year");
    // The type OIDs come from the schema: text=25, int8=20
    assert_eq!(
        find(&r, b'T').unwrap().columns(),
        vec![("name".to_string(), 25), ("year".to_string(), 20)]
    );
    assert_eq!(
        find(&r, b'D').unwrap().cells(),
        vec![Some("a".to_string()), Some("2024".to_string())]
    );

    // Errors map to SQLSTATE and the connection stays usable.
    let r = c.simple("get nosuch select x");
    assert_eq!(find(&r, b'E').unwrap().sqlstate().unwrap(), "42P01");
    assert!(find(&c.simple("get t select name"), b'D').is_some());
}

/// `Describe` must report the parameter count and the columns without
/// running the query. The previous version always answered `NoData` + an
/// empty parameter list.
#[test]
fn describe_reports_params_and_columns() {
    let h = trust_server();
    let mut c = Client::connect(h.port, "fenec", None).unwrap();
    c.simple("create collection t (name text, year int @hash, e vector<3> @hnsw(cosine))");
    c.simple("put t {name: \"a\", year: 2024, e: [1,0,0]}");

    let r = c.extended("get t select name where year >= $1", &["2000"], true);
    assert_eq!(find(&r, b't').unwrap().param_oids().len(), 1);
    assert_eq!(
        find(&r, b'T').unwrap().columns(),
        vec![("name".to_string(), 25)]
    );

    let r = c.extended(
        "get t select name where year >= $1 and name ~ $2",
        &["2000", "a"],
        true,
    );
    assert_eq!(find(&r, b't').unwrap().param_oids().len(), 2);

    // With `near` the `_score` column shows up in Describe too (float8 = 701).
    let r = c.extended("get t select name near e $1 limit 3", &["[1,0,0]"], true);
    assert_eq!(
        find(&r, b'T').unwrap().columns(),
        vec![("name".to_string(), 25), ("_score".to_string(), 701)]
    );

    // A statement that returns no rows -> NoData, but the parameter count is reported.
    let r = c.extended("put t {name: $1, year: 2030, e: [0,1,0]}", &["new"], true);
    assert_eq!(find(&r, b't').unwrap().param_oids().len(), 1);
    assert!(find(&r, b'n').is_some());
    assert!(find(&r, b'T').is_none());

    // An unknown collection: Describe does not error, Execute does.
    let r = c.extended("get nosuch select x", &[], true);
    assert!(find(&r, b'n').is_some());
    assert_eq!(find(&r, b'E').unwrap().sqlstate().unwrap(), "42P01");
}

/// Over the wire `count` is also a single row with a single column: the
/// parsers in psql and the drivers read the column type from Describe, so
/// `count` must be reported as int8 (20) -- there is no such field in the schema.
#[test]
fn count_is_an_int8_column() {
    let h = trust_server();
    let mut c = Client::connect(h.port, "fenec", None).unwrap();
    c.simple("create collection t (name text, year int)");
    c.simple("put t [{name: \"a\", year: 2024}, {name: \"b\", year: 2023}]");

    let r = c.extended("get t where year >= $1 count", &["2024"], true);
    assert_eq!(find(&r, b't').unwrap().param_oids().len(), 1);
    assert_eq!(
        find(&r, b'T').unwrap().columns(),
        vec![("count".to_string(), 20)]
    );
    assert_eq!(find(&r, b'D').unwrap().cells(), vec![Some("1".to_string())]);

    let r = c.simple("get t count");
    assert_eq!(find(&r, b'D').unwrap().cells(), vec![Some("2".to_string())]);
}

/// The column description sent with Describe must not be repeated on
/// Execute; when Describe is skipped it must be repeated (leniency).
#[test]
fn row_description_sent_exactly_once() {
    let h = trust_server();
    let mut c = Client::connect(h.port, "fenec", None).unwrap();
    c.simple("create collection t (name text)");
    c.simple("put t {name: \"a\"}");

    let with = c.extended("get t select name", &[], true);
    assert_eq!(
        with.iter().filter(|m| m.tag == b'T').count(),
        1,
        "Describe + Execute must not produce a second RowDescription: {:?}",
        tags(&with)
    );
    assert_eq!(tags(&with), vec!['1', 't', 'T', '2', 'D', 'C']);

    let without = c.extended("get t select name", &[], false);
    assert_eq!(
        without.iter().filter(|m| m.tag == b'T').count(),
        1,
        "when Describe is skipped the columns must arrive with Execute: {:?}",
        tags(&without)
    );
    assert_eq!(tags(&without), vec!['1', '2', 'T', 'D', 'C']);
}

#[test]
fn scram_auth_accepts_and_rejects() {
    let mut db = Database::new();
    db.install_plugin(&PgPlugin).unwrap();
    let cfg = Config {
        auth: Auth::parse("scram", "right-password").unwrap(),
        user: Some("fenec".into()),
        ..Config::default()
    };
    let h = start(cfg, db);

    let mut c = Client::connect(h.port, "fenec", Some("right-password")).expect("right password");
    assert!(find(&c.simple("SELECT 1"), b'D').is_some());

    let e = Client::connect(h.port, "fenec", Some("wrong")).unwrap_err();
    assert!(e.contains("password"), "unexpected error: {e}");

    // User name check
    let e = Client::connect(h.port, "someone-else", Some("right-password")).unwrap_err();
    assert!(e.contains("28000"), "unexpected error: {e}");
}

#[test]
fn cleartext_auth_still_works() {
    let mut db = Database::new();
    db.install_plugin(&PgPlugin).unwrap();
    let cfg = Config {
        auth: Auth::parse("cleartext", "secret").unwrap(),
        ..Config::default()
    };
    let h = start(cfg, db);
    assert!(Client::connect(h.port, "fenec", Some("secret")).is_ok());
    assert!(Client::connect(h.port, "fenec", Some("wrong")).is_err());
}

/// Read-only statements run under a shared lock: while the test holds a read
/// lock the client's read must still complete. With a single `Mutex` this
/// query would block until the lock was released.
#[test]
fn reads_share_the_lock() {
    let h = trust_server();
    let mut setup = Client::connect(h.port, "fenec", None).unwrap();
    setup.simple("create collection t (name text)");
    setup.simple("put t {name: \"a\"}");

    let held = h.db.read().unwrap(); // with a read lock open...
    let done = Arc::new(AtomicBool::new(false));
    let d2 = Arc::clone(&done);
    let port = h.port;
    let t = std::thread::spawn(move || {
        let mut c = Client::connect(port, "fenec", None).unwrap();
        let r = c.simple("get t select name");
        d2.store(true, Ordering::SeqCst);
        r
    });
    let start = Instant::now();
    while !done.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        done.load(Ordering::SeqCst),
        "the client's read blocked while a read lock was held"
    );
    let r = t.join().unwrap();
    assert_eq!(find(&r, b'D').unwrap().cells(), vec![Some("a".to_string())]);
    drop(held);
}

/// A write takes the exclusive lock: while the test holds the write lock the
/// client waits, and once `CancelRequest` arrives it returns `57014` with the
/// connection still usable.
#[test]
fn cancel_releases_a_waiting_query() {
    let h = trust_server();
    let mut setup = Client::connect(h.port, "fenec", None).unwrap();
    setup.simple("create collection t (name text)");
    setup.simple("put t {name: \"a\"}");

    let mut c = Client::connect(h.port, "fenec", None).unwrap();
    let (pid, secret) = (c.pid, c.secret);

    let held = h.db.write().unwrap(); // we hold the write lock
    c.send("put t {name: \"waiting\"}"); // waits on the lock
    std::thread::sleep(Duration::from_millis(50));
    send_cancel(h.port, pid, secret);

    let r = c.until_ready();
    assert_eq!(
        find(&r, b'E').unwrap().sqlstate().unwrap(),
        "57014",
        "a cancelled query must return 57014: {:?}",
        tags(&r)
    );
    drop(held);
    // The same connection keeps working.
    assert!(find(&c.simple("get t select name"), b'D').is_some());
}

/// A cancel arriving while idle, or with the wrong secret key, must not kill
/// the next query.
#[test]
fn stray_cancel_is_ignored() {
    let h = trust_server();
    let mut c = Client::connect(h.port, "fenec", None).unwrap();
    c.simple("create collection t (name text)");
    c.simple("put t {name: \"a\"}");

    send_cancel(h.port, c.pid, c.secret); // idle
    std::thread::sleep(Duration::from_millis(30));
    let r = c.simple("get t select name");
    assert!(find(&r, b'E').is_none(), "an idle cancel killed the query");

    send_cancel(h.port, c.pid, c.secret ^ 0x5555); // wrong key
    std::thread::sleep(Duration::from_millis(30));
    assert!(find(&c.simple("get t select name"), b'E').is_none());
}

/// `--sync always`: the file must be on disk after every write statement.
#[test]
fn sync_always_persists_every_write() {
    let dir = std::env::temp_dir().join(format!("fenecpg-sync-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("a.fenec");
    let _ = std::fs::remove_file(&path);

    let mut db = fenec_core::fs::open(&path).unwrap();
    db.install_plugin(&PgPlugin).unwrap();
    let h = start(
        Config {
            sync: SyncPolicy::Always,
            ..Config::default()
        },
        db,
    );
    let mut c = Client::connect(h.port, "fenec", None).unwrap();
    c.simple("create collection t (name text)");
    c.simple("put t {name: \"durable\"}");

    let size = std::fs::metadata(&path).unwrap().len();
    assert!(
        size > 8,
        "writes did not reach the disk (file is {size} bytes): the buffer is not flushed without `sync`"
    );

    // Open the same file independently and check the contents (as after a crash).
    let reopened = fenec_core::fs::open(&path).unwrap();
    let rows = reopened
        .query(&fenec_ql::parse("get t select name").unwrap()[0], &[])
        .unwrap();
    let rs = rows.rows().unwrap();
    assert_eq!(rs.rows.len(), 1);
    assert_eq!(rs.rows[0].values[0], Value::Text("durable".into()));
    let _ = std::fs::remove_file(&path);
}

/// The periodic syncer: once the interval passes, dirty writes reach the
/// disk on their own.
#[test]
fn interval_sync_flushes_in_background() {
    let dir = std::env::temp_dir().join(format!("fenecpg-int-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("b.fenec");
    let _ = std::fs::remove_file(&path);

    let mut db = fenec_core::fs::open(&path).unwrap();
    db.install_plugin(&PgPlugin).unwrap();
    let h = start(
        Config {
            sync: SyncPolicy::Interval(Duration::from_millis(50)),
            ..Config::default()
        },
        db,
    );
    let mut c = Client::connect(h.port, "fenec", None).unwrap();
    c.simple("create collection t (name text)");
    c.simple("put t {name: \"delayed\"}");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut size = 0;
    while Instant::now() < deadline {
        size = std::fs::metadata(&path).unwrap().len();
        if size > 8 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        size > 8,
        "the periodic syncer did not push the writes to disk"
    );
    assert!(
        !h.db.read().unwrap().is_dirty(),
        "still dirty after the sync"
    );
    let _ = std::fs::remove_file(&path);
}

/// Without TLS, listening outside loopback with no authentication is refused.
#[test]
fn remote_bind_without_auth_is_refused() {
    let db = Database::new();
    let cfg = Config {
        addr: "0.0.0.0:0".into(),
        ..Config::default()
    };
    let server = Server::new(Arc::new(RwLock::new(db)), cfg);
    let err = server.bind().expect_err("it should have been refused");
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);

    // With --insecure it can be opened deliberately.
    let cfg = Config {
        addr: "0.0.0.0:0".into(),
        insecure: true,
        ..Config::default()
    };
    let server = Server::new(Arc::new(RwLock::new(Database::new())), cfg);
    assert!(server.bind().is_ok());

    // With a password no protection is needed.
    let cfg = Config {
        addr: "0.0.0.0:0".into(),
        auth: Auth::parse("scram", "p").unwrap(),
        ..Config::default()
    };
    let server = Server::new(Arc::new(RwLock::new(Database::new())), cfg);
    assert!(server.bind().is_ok());
}

/// Concurrent HNSW search on the same index: the search buffer moved into
/// thread-local storage, so the results must match a serial run exactly.
#[test]
fn concurrent_vector_search_matches_serial() {
    let h = trust_server();
    let mut c = Client::connect(h.port, "fenec", None).unwrap();
    c.simple(
        "create collection v (name text, e vector<8> @hnsw(cosine, m=16, ef_construction=100))",
    );
    let mut seed = 1u64;
    let mut rnd = || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        ((seed >> 33) as f32) / (u32::MAX as f32 / 2.0)
    };
    let mut docs = String::new();
    for i in 0..2000 {
        let v: Vec<String> = (0..8).map(|_| format!("{:.4}", rnd())).collect();
        docs.push_str(&format!("{{name: \"v{i}\", e: [{}]}} ", v.join(",")));
    }
    c.simple(&format!("put v {docs}"));

    let queries: Vec<String> = (0..12)
        .map(|_| {
            let v: Vec<String> = (0..8).map(|_| format!("{:.4}", rnd())).collect();
            format!("[{}]", v.join(","))
        })
        .collect();

    let serial: Vec<Vec<Option<String>>> = queries
        .iter()
        .map(|q| {
            c.extended("get v select name near e $1 limit 5", &[q], false)
                .iter()
                .filter(|m| m.tag == b'D')
                .map(|m| m.cells()[0].clone())
                .collect()
        })
        .collect();

    let port = h.port;
    let handles: Vec<_> = queries
        .iter()
        .cloned()
        .map(|q| {
            std::thread::spawn(move || {
                let mut c = Client::connect(port, "fenec", None).unwrap();
                c.extended("get v select name near e $1 limit 5", &[&q], false)
                    .iter()
                    .filter(|m| m.tag == b'D')
                    .map(|m| m.cells()[0].clone())
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let parallel: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    assert_eq!(serial.len(), parallel.len());
    for (i, (s, p)) in serial.iter().zip(&parallel).enumerate() {
        assert!(!s.is_empty(), "query {i} returned nothing");
        assert_eq!(
            s, p,
            "query {i}: the parallel search differs from the serial one"
        );
    }
}

/// `timestamp` fields are reported as `timestamptz` (OID 1184) on the wire
/// and go out in PostgreSQL's own output format -- not ISO-8601's `T`/`Z`
/// spelling, because client parsers expect the server format.
#[test]
fn timestamps_use_pg_oid_and_text_format() {
    let h = trust_server();
    let mut c = Client::connect(h.port, "fenec", None).unwrap();
    c.simple("create collection event (name text, t timestamp @hash)");
    c.simple("put event {name: \"a\", t: \"2026-09-19T12:34:56.789Z\"}");

    let r = c.simple("get event select name, t");
    assert_eq!(
        find(&r, b'T').unwrap().columns(),
        vec![("name".to_string(), 25), ("t".to_string(), 1184)]
    );
    assert_eq!(
        find(&r, b'D').unwrap().cells(),
        vec![
            Some("a".to_string()),
            Some("2026-09-19 12:34:56.789+00".to_string())
        ]
    );

    // The parameter arrives as text and is parsed at the schema boundary.
    let r = c.extended(
        "put event {name: $1, t: $2}",
        &["b", "2020-01-02 03:04:05+02"],
        true,
    );
    assert!(find(&r, b'E').is_none(), "the parameterised write errored");

    // A range query against a text literal.
    let r = c.simple("get event select name where t < \"2021-01-01\"");
    assert_eq!(find(&r, b'D').unwrap().cells(), vec![Some("b".to_string())]);

    // Describe must report the same OID.
    let r = c.extended("get event select t where name = $1", &["a"], true);
    assert_eq!(
        find(&r, b'T').unwrap().columns(),
        vec![("t".to_string(), 1184)]
    );

    // The pg_typeof plugin recognises the timestamp.
    let r = c.simple("get event select name where t >= \"2026-01-01\"");
    assert_eq!(find(&r, b'D').unwrap().cells(), vec![Some("a".to_string())]);
}

// --------------------------------------------------------------- resources

/// An empty database + the plugin: no schema is needed to exercise the
/// resource limits, but `PgPlugin` is loaded anyway for the compat queries.
fn bare_db() -> Database {
    let mut db = Database::new();
    db.install_plugin(&PgPlugin).unwrap();
    db
}

/// Connection ceiling: a connection above it is refused with `53300`, a live
/// session is unaffected, and a closed connection frees its slot.
#[test]
fn connection_limit_refuses_extra_sessions() {
    let h = start(
        Config {
            max_connections: 1,
            ..Config::default()
        },
        bare_db(),
    );

    let mut first = Client::connect(h.port, "fenec", None).unwrap();
    let err = Client::connect(h.port, "fenec", None).expect_err("the ceiling was not applied");
    assert!(err.contains("53300"), "expected 53300, got: {err}");

    // A refused connection does not affect the live one.
    assert!(find(&first.simple("collections"), b'E').is_none());

    // The counter drops when the session thread ends, so the close is seen
    // "shortly after" rather than immediately.
    drop(first);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match Client::connect(h.port, "fenec", None) {
            Ok(_) => break,
            Err(e) => {
                assert!(Instant::now() < deadline, "no slot freed up: {e}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// An idle session is closed and the client sees why -- a connection that
/// drops silently cannot be debugged.
#[test]
fn idle_timeout_closes_the_session() {
    let h = start(
        Config {
            idle_timeout: Some(Duration::from_millis(200)),
            ..Config::default()
        },
        bare_db(),
    );
    let mut c = Client::connect(h.port, "fenec", None).unwrap();

    let m = c
        .read_msg()
        .expect("the server closed without giving a reason");
    assert_eq!(m.tag, b'E');
    assert_eq!(m.sqlstate().as_deref(), Some("57P05"), "{:?}", m.message());
}

/// A message over the ceiling: since the length is read before the body, the
/// body is never allocated; the client gets `54000`.
#[test]
fn oversized_message_is_rejected() {
    let h = start(
        Config {
            max_message: 1 << 10,
            ..Config::default()
        },
        bare_db(),
    );
    let mut c = Client::connect(h.port, "fenec", None).unwrap();

    c.send(&format!("get t select {}", "a".repeat(2000)));
    let m = c.read_msg().expect("no response");
    assert_eq!(m.tag, b'E');
    assert_eq!(m.sqlstate().as_deref(), Some("54000"), "{:?}", m.message());
}

/// The startup packet ceiling. Since the packet is allocated from the
/// *declared* length, the limit has to be applied before reading: the test
/// declares 20 000 bytes and sends 8. If the server waits for the body, this
/// test hangs.
#[test]
fn oversized_startup_packet_is_rejected() {
    let h = trust_server();
    let mut s = TcpStream::connect(("127.0.0.1", h.port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

    let mut pkt = 20_000i32.to_be_bytes().to_vec();
    pkt.extend_from_slice(&196_608i32.to_be_bytes());
    s.write_all(&pkt).unwrap();

    let mut head = [0u8; 5];
    s.read_exact(&mut head)
        .expect("the server waited for the body");
    let len = i32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
    let mut body = vec![0u8; len - 4];
    s.read_exact(&mut body).unwrap();
    let m = Msg { tag: head[0], body };
    assert_eq!(m.tag, b'E');
    assert_eq!(m.sqlstate().as_deref(), Some("54000"), "{:?}", m.message());
}

/// Data ceiling: above it writes stop with `53200`, but reads and the
/// recovery path (`del` + `compact`) stay open. Hitting the ceiling with no
/// way out would be no better than a cgroup OOM.
#[test]
fn memory_cap_stops_writes_and_leaves_a_way_out() {
    let h = start(
        Config {
            max_memory: 8 << 10,
            ..Config::default()
        },
        bare_db(),
    );
    let mut c = Client::connect(h.port, "fenec", None).unwrap();
    assert!(find(&c.simple("create collection t (name text)"), b'E').is_none());

    let row = format!(r#"put t {{name: "{}"}}"#, "x".repeat(64));
    let mut refused = None;
    for _ in 0..500 {
        if let Some(e) = find(&c.simple(&row), b'E') {
            refused = Some(e.clone());
            break;
        }
    }
    let e = refused.expect("the ceiling was not applied");
    assert_eq!(e.sqlstate().as_deref(), Some("53200"), "{:?}", e.message());

    // Reads are unaffected.
    assert!(find(&c.simple("get t select name limit 1"), b'E').is_none());

    // The way out: delete and compact work, and then writes are accepted
    // again.
    assert!(find(&c.simple("del t"), b'E').is_none());
    assert!(find(&c.simple("compact"), b'E').is_none());
    assert!(
        find(&c.simple(&row), b'E').is_none(),
        "room was freed but writes are still refused"
    );
}

/// A deep expression does not take the server down. A stack overflow is not
/// a catchable panic but an `abort` of the process: the deepest expression
/// below the limit must run in a real session thread, and the one above it
/// must return an error and leave the connection alive.
#[test]
fn deep_expression_does_not_kill_the_server() {
    let h = start(Config::default(), bare_db());
    let mut c = Client::connect(h.port, "fenec", None).unwrap();
    assert!(find(&c.simple("create collection t (n int)"), b'E').is_none());

    let nested = |d: usize| format!("get t where {}n = 1{}", "(".repeat(d), ")".repeat(d));

    // Above the limit: an error, but the connection lives.
    let r = c.simple(&nested(fenec_ql::MAX_EXPR_DEPTH + 64));
    let e = find(&r, b'E').expect("the depth limit was not applied");
    assert_eq!(e.sqlstate().as_deref(), Some("42601"), "{:?}", e.message());
    assert!(e.message().unwrap_or_default().contains("too deep"));

    // The expression just below the limit runs: the session stack is enough.
    let r = c.simple(&nested(fenec_ql::MAX_EXPR_DEPTH - 1));
    assert!(
        find(&r, b'E').is_none(),
        "{:?}",
        find(&r, b'E').and_then(|e| e.message())
    );

    // The server still answers.
    assert!(find(&c.simple("collections"), b'E').is_none());
}

/// A PostgreSQL row is flat, so `lookup` arrives widened the way a join
/// presents it -- and the child's columns have to be described with their
/// real type OIDs. The fallback types anything it cannot place as `text`,
/// and `Describe` answers before the query runs, so a client that learned
/// the shape there would have no chance to correct it later.
#[test]
fn lookup_is_flattened_and_its_child_columns_are_typed() {
    let h = trust_server();
    let mut c = Client::connect(h.port, "fenec", None).unwrap();
    c.simple("create collection products (name text, price int)");
    c.simple("create collection reviews (product_id int @hash, stars int, body text)");
    c.simple(r#"put products [{name: "Kahve", price: 12000}, {name: "Demlik", price: 34000}]"#);
    c.simple(r#"put reviews [{product_id: 1, stars: 5, body: "guzel"}, {product_id: 1, stars: 3, body: "idare"}]"#);

    let sql = "get products select name lookup reviews on product_id select stars, body";
    let r = c.simple(sql);
    // text=25, int8=20 -- not the `text` everything degrades to.
    assert_eq!(
        find(&r, b'T').unwrap().columns(),
        vec![
            ("name".to_string(), 25),
            ("reviews.stars".to_string(), 20),
            ("reviews.body".to_string(), 25),
        ]
    );
    let rows: Vec<Vec<Option<String>>> = r
        .iter()
        .filter(|m| m.tag == b'D')
        .map(|m| m.cells())
        .collect();
    assert_eq!(
        rows,
        vec![
            vec![
                Some("Kahve".to_string()),
                Some("5".to_string()),
                Some("guzel".to_string())
            ],
            vec![
                Some("Kahve".to_string()),
                Some("3".to_string()),
                Some("idare".to_string())
            ],
            // A parent with no children keeps its row: the page is the
            // parents, so the child columns are null rather than absent.
            vec![Some("Demlik".to_string()), None, None],
        ]
    );

    // The same shape through `Describe`, which answers without executing.
    let r = c.extended(sql, &[], true);
    assert_eq!(
        find(&r, b'T').unwrap().columns(),
        vec![
            ("name".to_string(), 25),
            ("reviews.stars".to_string(), 20),
            ("reviews.body".to_string(), 25),
        ]
    );

    // A parameter inside the child's `where` is part of the same statement.
    // `Describe` reports the count before the query runs, so a client told
    // there were none would send none and the query would fail on an
    // unbound `$1`.
    let r = c.extended(
        "get products select name lookup reviews on product_id select stars where stars >= $1",
        &["4"],
        true,
    );
    assert_eq!(find(&r, b't').unwrap().param_oids().len(), 1);
    let rows: Vec<Vec<Option<String>>> = r
        .iter()
        .filter(|m| m.tag == b'D')
        .map(|m| m.cells())
        .collect();
    assert_eq!(
        rows,
        vec![
            vec![Some("Kahve".to_string()), Some("5".to_string())],
            vec![Some("Demlik".to_string()), None],
        ]
    );
}
