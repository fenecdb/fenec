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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
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

    /// Parse, Bind and Execute of one statement over the unnamed statement
    /// and portal, with no Sync after them: one step of a pipeline.
    fn step(sql: &str) -> Vec<u8> {
        let mut out = Vec::new();
        let mut p = Vec::new();
        cstr(&mut p, "");
        cstr(&mut p, sql);
        p.extend_from_slice(&0i16.to_be_bytes());
        out.extend_from_slice(&framed(b'P', &p));
        let mut bind = Vec::new();
        cstr(&mut bind, "");
        cstr(&mut bind, "");
        // no parameter formats, no parameters, no result formats
        bind.extend_from_slice(&[0; 6]);
        out.extend_from_slice(&framed(b'B', &bind));
        let mut e = Vec::new();
        cstr(&mut e, "");
        e.extend_from_slice(&0i32.to_be_bytes());
        out.extend_from_slice(&framed(b'E', &e));
        out
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

/// An aggregate's column is typed from what it answers: a count and an
/// int's sum as int8, an average as float8, a min or max as its field.
#[test]
fn aggregate_columns_are_typed() {
    let h = trust_server();
    let mut c = Client::connect(h.port, "fenec", None).unwrap();
    c.simple("create collection t (g text, n int, x float)");
    c.simple("put t [{g: \"a\", n: 2, x: 0.5}, {g: \"a\", n: 4}, {g: \"b\", n: 1}]");

    let r = c.extended(
        "get t select g, count(*), sum(n), avg(n), max(x) where n >= $1 group g",
        &["1"],
        true,
    );
    assert_eq!(
        find(&r, b'T').unwrap().columns(),
        vec![
            ("g".to_string(), 25),
            ("count".to_string(), 20),
            ("sum(n)".to_string(), 20),
            ("avg(n)".to_string(), 701),
            ("max(x)".to_string(), 701),
        ]
    );
    let rows: Vec<Vec<Option<String>>> = r
        .iter()
        .filter(|m| m.tag == b'D')
        .map(|m| m.cells())
        .collect();
    let s = |v: &str| Some(v.to_string());
    assert_eq!(
        rows,
        vec![
            vec![s("a"), s("2"), s("6"), s("3"), s("0.5")],
            vec![s("b"), s("1"), s("1"), s("1"), None],
        ]
    );
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

/// `match` ranks as `near` does, and its score travels the same way: the
/// column was described for `near` alone, so over the wire a BM25 ranking
/// arrived without the number it was ranked by.
#[test]
fn match_carries_its_score() {
    let h = trust_server();
    let mut c = Client::connect(h.port, "fenec", None).unwrap();
    c.simple("create collection d (body text @text)");
    c.simple("put d {body: \"the quick brown fox\"}");
    c.simple("put d {body: \"a lazy dog\"}");
    let columns = vec![("body".to_string(), 25), ("_score".to_string(), 701)];

    let r = c.extended("get d select body match body $1 limit 5", &["fox"], true);
    assert_eq!(find(&r, b'T').unwrap().columns(), columns);
    let cells = find(&r, b'D').unwrap().cells();
    assert_eq!(cells[0].as_deref(), Some("the quick brown fox"));
    assert!(cells[1].as_deref().unwrap().parse::<f32>().unwrap() > 0.0);

    let r = c.simple("get d select body match body \"fox\"");
    assert_eq!(find(&r, b'T').unwrap().columns(), columns);
    assert_eq!(find(&r, b'D').unwrap().cells().len(), 2);
}

/// An error in the extended protocol drops everything up to the next Sync,
/// as PostgreSQL does. A pipelining client -- libpq's pipeline mode, pgx's
/// batches, JDBC's -- writes off what it queued behind the failure and
/// reads no answer for it; run anyway, a write the client counted as
/// aborted was made, and its answers were read as the next query's.
#[test]
fn an_error_skips_the_rest_of_the_pipeline_to_the_sync() {
    let h = trust_server();
    let mut c = Client::connect(h.port, "fenec", None).unwrap();
    c.simple("create collection t (name text)");
    let count = |c: &mut Client| {
        let r = c.simple("get t count");
        find(&r, b'D').unwrap().cells()
    };

    let mut pipe = Client::step("get nosuch select x");
    pipe.extend(Client::step("put t {name: \"behind the error\"}"));
    pipe.extend(framed(b'S', &[]));
    c.s.write_all(&pipe).unwrap();
    let r = c.until_ready();
    assert_eq!(tags(&r), vec!['1', '2', 'E']);
    assert_eq!(find(&r, b'E').unwrap().sqlstate().unwrap(), "42P01");
    // One ReadyForQuery for the one Sync, so this answer is this query's.
    assert_eq!(count(&mut c), vec![Some("0".to_string())]);

    // The error goes out as it happens, not at the Sync: a client waiting
    // on it before sending more reads it. What follows is still dropped,
    // a simple query too.
    c.s.write_all(&Client::step("get nosuch select x")).unwrap();
    for want in *b"12E" {
        assert_eq!(c.read_msg().unwrap().tag, want);
    }
    let mut pipe = Client::step("put t {name: \"dropped\"}");
    pipe.extend(framed(b'H', &[]));
    let mut q = Vec::new();
    cstr(&mut q, "put t {name: \"dropped too\"}");
    pipe.extend(framed(b'Q', &q));
    pipe.extend(framed(b'S', &[]));
    c.s.write_all(&pipe).unwrap();
    assert_eq!(tags(&c.until_ready()), Vec::<char>::new());
    assert_eq!(count(&mut c), vec![Some("0".to_string())]);

    // Past the Sync the session is what it was.
    let r = c.extended("put t {name: \"x\"}", &[], false);
    assert_eq!(tags(&r), vec!['1', '2', 'C']);
    assert_eq!(count(&mut c), vec![Some("1".to_string())]);
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

/// A chain widens once per level, and every level has to be described --
/// a block left out would be typed by the caller's fallback, which is the
/// exact failure the single-level version of this test pins.
///
/// The rows are one root-to-leaf path each, and a level that ran out fills
/// its own columns and every column below it with nulls. That is a chain of
/// left joins, which is the only rendering a wire with no nested row can be
/// given; what it cannot show is the per-level `limit`, which is why the
/// nesting is what the other transports carry.
#[test]
fn a_chained_lookup_is_flattened_level_by_level() {
    let h = trust_server();
    let mut c = Client::connect(h.port, "fenec", None).unwrap();
    c.simple("create collection shops (name text)");
    c.simple("create collection orders (shop_id int @hash, code text)");
    c.simple("create collection lines (order_id int @hash, item text, qty int)");
    c.simple(r#"put shops [{name: "Merkez"}, {name: "Depo"}]"#);
    c.simple(r#"put orders [{shop_id: 1, code: "A"}, {shop_id: 1, code: "B"}]"#);
    c.simple(r#"put lines [{order_id: 1, item: "kahve", qty: 2}]"#);

    let sql = "get shops select name lookup orders on shop_id select code                lookup lines on order_id select item, qty";
    let want = vec![
        ("name".to_string(), 25),
        ("orders.code".to_string(), 25),
        ("lines.item".to_string(), 25),
        // int8, not the `text` an undescribed level would degrade to.
        ("lines.qty".to_string(), 20),
    ];
    let r = c.simple(sql);
    assert_eq!(find(&r, b'T').unwrap().columns(), want);
    let rows: Vec<Vec<Option<String>>> = r
        .iter()
        .filter(|m| m.tag == b'D')
        .map(|m| m.cells())
        .collect();
    assert_eq!(
        rows,
        vec![
            vec![
                Some("Merkez".to_string()),
                Some("A".to_string()),
                Some("kahve".to_string()),
                Some("2".to_string()),
            ],
            // Order B has no lines: the leaf block goes null.
            vec![
                Some("Merkez".to_string()),
                Some("B".to_string()),
                None,
                None,
            ],
            // Depo has no orders: both blocks below it go null.
            vec![Some("Depo".to_string()), None, None, None],
        ]
    );

    // And the same shape from `Describe`, which answers without executing.
    let r = c.extended(sql, &[], true);
    assert_eq!(find(&r, b'T').unwrap().columns(), want);
}

// ------------------------------------------------------------ transactions

/// The status byte of the ReadyForQuery that closes a response.
fn status(msgs: &[Msg]) -> u8 {
    let z = msgs.last().expect("a response");
    assert_eq!(z.tag, b'Z', "a response ends with ReadyForQuery");
    z.body[0]
}

/// The names in `t`, in id order.
fn names(c: &mut Client) -> Vec<String> {
    c.simple("get t select name")
        .iter()
        .filter(|m| m.tag == b'D')
        .map(|m| m.cells()[0].clone().unwrap())
        .collect()
}

/// Whether the server answers within `ms`: a statement waiting on another
/// session's transaction does not.
fn answers_within(c: &mut Client, ms: u64) -> bool {
    c.s.set_read_timeout(Some(Duration::from_millis(ms)))
        .unwrap();
    let got = c.s.peek(&mut [0u8; 1]).is_ok();
    c.s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    got
}

/// `COMMIT` lands a transaction's writes and `ROLLBACK` puts them back. In
/// between another session waits rather than read them: they may yet be
/// put back.
#[test]
fn a_transaction_lands_at_commit_or_not_at_all() {
    let h = trust_server();
    let mut c = Client::connect(h.port, "fenec", None).unwrap();
    let mut other = Client::connect(h.port, "fenec", None).unwrap();
    c.simple("create collection t (name text)");

    let r = c.simple("BEGIN");
    assert_eq!(find(&r, b'C').unwrap().tag_text(), "BEGIN");
    assert_eq!(
        status(&r),
        b'T',
        "inside a transaction the session reports T"
    );
    c.simple("put t {name: \"a\"}");
    let r = c.simple("put t {name: \"b\"}");
    assert_eq!(status(&r), b'T');
    assert_eq!(
        names(&mut c),
        ["a", "b"],
        "a transaction reads its own writes"
    );
    other.send("get t count");
    assert!(
        !answers_within(&mut other, 300),
        "a read went past an open transaction"
    );

    let r = c.simple("ROLLBACK");
    assert_eq!(find(&r, b'C').unwrap().tag_text(), "ROLLBACK");
    assert_eq!(status(&r), b'I');
    let r = other.until_ready();
    assert_eq!(find(&r, b'D').unwrap().cells(), [Some("0".to_string())]);
    assert!(names(&mut c).is_empty());

    c.simple("BEGIN");
    c.simple("put t {name: \"c\"}");
    let r = c.simple("COMMIT");
    assert_eq!(find(&r, b'C').unwrap().tag_text(), "COMMIT");
    assert_eq!(status(&r), b'I');
    assert_eq!(names(&mut other), ["c"]);

    // The extended protocol sees the same, so a driver that reads its state
    // from ReadyForQuery sends its own COMMIT.
    assert_eq!(status(&c.extended("BEGIN", &[], false)), b'T');
    let r = c.extended("put t {name: $1}", &["d"], false);
    assert_eq!(status(&r), b'T');
    let r = c.extended("COMMIT", &[], false);
    assert_eq!(find(&r, b'C').unwrap().tag_text(), "COMMIT");
    assert_eq!(status(&r), b'I');
    assert_eq!(names(&mut other), ["c", "d"]);
}

/// Until its first write a transaction reads what others commit, statement
/// by statement, and holds nobody up.
#[test]
fn a_transaction_holds_the_database_from_its_first_write() {
    let h = trust_server();
    let mut c = Client::connect(h.port, "fenec", None).unwrap();
    let mut other = Client::connect(h.port, "fenec", None).unwrap();
    c.simple("create collection t (name text)");

    c.simple("BEGIN");
    assert!(names(&mut c).is_empty());
    let r = other.simple("put t {name: \"committed\"}");
    assert_eq!(status(&r), b'I');
    assert_eq!(names(&mut c), ["committed"]);
    c.simple("put t {name: \"mine\"}");
    other.send("put t {name: \"late\"}");
    assert!(
        !answers_within(&mut other, 300),
        "a write went past an open transaction"
    );
    c.simple("COMMIT");
    assert_eq!(
        find(&other.until_ready(), b'C').unwrap().tag_text(),
        "INSERT 0 1"
    );
    assert_eq!(names(&mut c), ["committed", "mine", "late"]);
}

/// A statement that fails in a transaction fails the transaction, as in
/// PostgreSQL: what it wrote is put back at once, and every statement up
/// to its end is refused -- a client that went on would take them for
/// landed.
#[test]
fn an_error_fails_the_transaction_until_its_end() {
    let h = trust_server();
    let mut c = Client::connect(h.port, "fenec", None).unwrap();
    c.simple("create collection t (name text)");

    c.simple("BEGIN");
    c.simple("put t {name: \"a\"}");
    let r = c.simple("put t {name: 1}");
    assert_eq!(find(&r, b'E').unwrap().sqlstate().unwrap(), "42804");
    assert_eq!(status(&r), b'E');
    let r = c.simple("put t {name: \"b\"}");
    assert_eq!(find(&r, b'E').unwrap().sqlstate().unwrap(), "25P02");
    assert_eq!(status(&r), b'E');
    // Nobody waits on it: the lock went with the failure.
    let mut other = Client::connect(h.port, "fenec", None).unwrap();
    assert!(names(&mut other).is_empty());

    // A failed transaction's COMMIT is a ROLLBACK, as PostgreSQL answers it.
    let r = c.simple("COMMIT");
    assert_eq!(find(&r, b'C').unwrap().tag_text(), "ROLLBACK");
    assert_eq!(status(&r), b'I');
    assert!(names(&mut c).is_empty());

    // A read that fails fails it too.
    c.simple("BEGIN");
    let r = c.simple("get nosuch");
    assert_eq!(find(&r, b'E').unwrap().sqlstate().unwrap(), "42P01");
    assert_eq!(status(&r), b'E');
    let r = c.simple("ROLLBACK");
    assert_eq!(find(&r, b'C').unwrap().tag_text(), "ROLLBACK");
    assert_eq!(status(&r), b'I');
}

/// With nothing to put back, ROLLBACK and COMMIT are what they say; outside
/// a transaction they warn, as PostgreSQL's do, and succeed: pools send
/// them on every check-in.
#[test]
fn transaction_control_outside_a_transaction_warns() {
    let h = trust_server();
    let mut c = Client::connect(h.port, "fenec", None).unwrap();
    for q in ["ROLLBACK", "COMMIT"] {
        let r = c.simple(q);
        assert_eq!(find(&r, b'N').unwrap().sqlstate().unwrap(), "25P01");
        assert_eq!(find(&r, b'C').unwrap().tag_text(), q);
        assert_eq!(status(&r), b'I');
    }
    c.simple("BEGIN");
    let r = c.simple("BEGIN");
    assert_eq!(find(&r, b'N').unwrap().sqlstate().unwrap(), "25001");
    assert_eq!(status(&r), b'T');
    assert_eq!(
        find(&c.simple("ROLLBACK"), b'C').unwrap().tag_text(),
        "ROLLBACK"
    );
}

/// A pipeline of the extended protocol is one block up to its Sync, as
/// PostgreSQL runs one as a transaction: a failure in it puts back what came
/// before it in the pipeline.
#[test]
fn a_pipeline_lands_whole_at_its_sync() {
    let h = trust_server();
    let mut c = Client::connect(h.port, "fenec", None).unwrap();
    c.simple("create collection t (name text)");

    let mut pipe = Client::step("put t {name: \"a\"}");
    pipe.extend(Client::step("put t {name: \"b\"}"));
    pipe.extend(Client::step("put t {name: 1}"));
    pipe.extend(Client::step("put t {name: \"skipped\"}"));
    pipe.extend(framed(b'S', &[]));
    c.s.write_all(&pipe).unwrap();
    let r = c.until_ready();
    assert_eq!(tags(&r), vec!['1', '2', 'C', '1', '2', 'C', '1', '2', 'E']);
    assert_eq!(find(&r, b'E').unwrap().sqlstate().unwrap(), "42804");
    assert_eq!(status(&r), b'I');
    assert!(
        names(&mut c).is_empty(),
        "what came before the error landed"
    );

    let mut pipe = Client::step("put t {name: \"a\"}");
    pipe.extend(Client::step("put t {name: \"b\"}"));
    pipe.extend(framed(b'S', &[]));
    c.s.write_all(&pipe).unwrap();
    assert_eq!(tags(&c.until_ready()), vec!['1', '2', 'C', '1', '2', 'C']);
    assert_eq!(names(&mut c), ["a", "b"]);
}

/// A create, a drop and a create index are writes of the transaction like
/// any other: put back by its ROLLBACK, landed by its COMMIT, and seen by
/// nobody before. A compact rewrites the file: it runs on its own before the
/// first write, and is refused after one.
#[test]
fn a_schema_change_in_a_transaction_lands_with_it_or_not_at_all() {
    let h = trust_server();
    let mut c = Client::connect(h.port, "fenec", None).unwrap();
    let mut other = Client::connect(h.port, "fenec", None).unwrap();

    c.simple("BEGIN");
    let r = c.simple("create collection t (name text)");
    assert_eq!(find(&r, b'C').unwrap().tag_text(), "CREATE TABLE");
    c.simple("put t {name: \"a\"}");
    c.simple("create index on t (name) @hash");
    assert_eq!(names(&mut c), ["a"]);
    other.send("get t count");
    assert!(
        !answers_within(&mut other, 300),
        "a read went past an open transaction"
    );
    let r = c.simple("ROLLBACK");
    assert_eq!(find(&r, b'C').unwrap().tag_text(), "ROLLBACK");
    assert!(find(&r, b'E').is_none(), "{r:?}");
    assert_eq!(status(&r), b'I');
    let r = other.until_ready();
    assert_eq!(find(&r, b'E').unwrap().sqlstate().unwrap(), "42P01");

    // After a write too, landed by the COMMIT.
    c.simple("create collection t (name text)");
    c.simple("BEGIN");
    c.simple("put t {name: \"b\"}");
    let r = c.simple("create collection u (x int)");
    assert_eq!(find(&r, b'C').unwrap().tag_text(), "CREATE TABLE");
    assert_eq!(status(&r), b'T');
    c.simple("put u {x: 1}");
    c.simple("drop collection t");
    let r = c.simple("COMMIT");
    assert_eq!(find(&r, b'C').unwrap().tag_text(), "COMMIT");
    let r = other.simple("get u count");
    assert_eq!(find(&r, b'D').unwrap().cells(), [Some("1".to_string())]);
    let r = other.simple("get t");
    assert_eq!(find(&r, b'E').unwrap().sqlstate().unwrap(), "42P01");

    // A compact before the first write runs on its own; after one it is
    // refused.
    c.simple("BEGIN");
    let r = c.simple("compact");
    assert!(find(&r, b'E').is_none(), "{r:?}");
    c.simple("put u {x: 2}");
    let r = c.simple("compact");
    assert_eq!(find(&r, b'E').unwrap().sqlstate().unwrap(), "25001");
    assert_eq!(status(&r), b'E');
    c.simple("ROLLBACK");
    let r = other.simple("get u count");
    assert_eq!(find(&r, b'D').unwrap().cells(), [Some("1".to_string())]);
}

/// READ ONLY refuses a write; SERIALIZABLE takes the lock at its first
/// statement, so nothing it read changes before it ends.
#[test]
fn read_only_and_serializable_transactions() {
    let h = trust_server();
    let mut c = Client::connect(h.port, "fenec", None).unwrap();
    let mut other = Client::connect(h.port, "fenec", None).unwrap();
    c.simple("create collection t (name text)");

    c.simple("BEGIN READ ONLY");
    let r = c.simple("put t {name: \"a\"}");
    assert_eq!(find(&r, b'E').unwrap().sqlstate().unwrap(), "25006");
    assert_eq!(status(&r), b'E');
    c.simple("ROLLBACK");

    c.simple("BEGIN ISOLATION LEVEL SERIALIZABLE");
    assert!(names(&mut c).is_empty());
    other.send("put t {name: \"late\"}");
    assert!(
        !answers_within(&mut other, 300),
        "a write went past a serializable transaction"
    );
    assert!(names(&mut c).is_empty(), "what it read changed under it");
    // Its isolation is settled by its first statement.
    let r = c.simple("SET TRANSACTION ISOLATION LEVEL READ COMMITTED");
    assert_eq!(find(&r, b'E').unwrap().sqlstate().unwrap(), "25001");
    c.simple("ROLLBACK");
    assert_eq!(
        find(&other.until_ready(), b'C').unwrap().tag_text(),
        "INSERT 0 1"
    );

    // The session's default, as JDBC's setTransactionIsolation sets it.
    c.simple("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY");
    c.simple("BEGIN");
    let r = c.simple("put t {name: \"b\"}");
    assert_eq!(find(&r, b'E').unwrap().sqlstate().unwrap(), "25006");
    c.simple("ROLLBACK");
    assert_eq!(names(&mut c), ["late"]);
}

/// `ROLLBACK TO` puts back the writes after its savepoint and nothing
/// before it, and the transaction goes on; the savepoint stays, and the
/// ones after it are over. `RELEASE` forgets a savepoint and the ones after
/// it, and keeps their writes.
#[test]
fn a_savepoint_puts_back_only_what_came_after_it() {
    let h = trust_server();
    let mut c = Client::connect(h.port, "fenec", None).unwrap();
    let mut other = Client::connect(h.port, "fenec", None).unwrap();
    c.simple("create collection t (name text)");

    c.simple("BEGIN");
    c.simple("put t {name: \"a\"}");
    let r = c.simple("SAVEPOINT s1");
    assert_eq!(find(&r, b'C').unwrap().tag_text(), "SAVEPOINT");
    assert_eq!(status(&r), b'T');
    c.simple("put t {name: \"b\"}");
    c.simple("SAVEPOINT s2");
    c.simple("put t {name: \"c\"}");
    let r = c.simple("ROLLBACK TO s2");
    assert_eq!(find(&r, b'C').unwrap().tag_text(), "ROLLBACK");
    assert_eq!(status(&r), b'T');
    assert_eq!(names(&mut c), ["a", "b"]);
    c.simple("put t {name: \"d\"}");
    c.simple("ROLLBACK TO SAVEPOINT s1");
    assert_eq!(names(&mut c), ["a"]);
    // Taken back to again, as often as asked; the one after it is over.
    c.simple("put t {name: \"e\"}");
    c.simple("ROLLBACK TRANSACTION TO SAVEPOINT s1");
    assert_eq!(names(&mut c), ["a"]);
    let r = c.simple("RELEASE s2");
    assert_eq!(find(&r, b'E').unwrap().sqlstate().unwrap(), "3B001");
    assert_eq!(status(&r), b'E');
    c.simple("ROLLBACK TO s1");

    // The extended protocol, as psycopg's nested transactions send it.
    c.simple("put t {name: \"f\"}");
    let r = c.extended("SAVEPOINT \"_pg3_1\"", &[], true);
    assert_eq!(find(&r, b'C').unwrap().tag_text(), "SAVEPOINT");
    c.extended("put t {name: $1}", &["g"], false);
    let r = c.extended("RELEASE \"_pg3_1\"", &[], false);
    assert_eq!(find(&r, b'C').unwrap().tag_text(), "RELEASE");
    assert_eq!(status(&r), b'T');
    c.extended("SAVEPOINT \"_pg3_1\"", &[], false);
    c.extended("put t {name: $1}", &["h"], false);
    c.extended("ROLLBACK TO \"_pg3_1\"", &[], false);
    let r = c.extended("RELEASE \"_pg3_1\"", &[], false);
    assert_eq!(status(&r), b'T');
    // RELEASE kept the writes of the savepoints it forgot: `g` stays.
    assert_eq!(names(&mut c), ["a", "f", "g"]);
    other.send("get t count");
    assert!(
        !answers_within(&mut other, 300),
        "a read went past an open transaction"
    );
    let r = c.simple("COMMIT");
    assert_eq!(find(&r, b'C').unwrap().tag_text(), "COMMIT");
    assert_eq!(status(&r), b'I');
    let r = other.until_ready();
    assert_eq!(find(&r, b'D').unwrap().cells(), [Some("3".to_string())]);
    assert_eq!(names(&mut other), ["a", "f", "g"]);
}

/// A transaction that failed is taken back to a savepoint before the
/// failure and goes on, as PostgreSQL's is: its block and its lock are kept
/// meanwhile, since the writes before the savepoint are still to land.
#[test]
fn a_failed_transaction_is_taken_back_to_a_savepoint() {
    let h = trust_server();
    let mut c = Client::connect(h.port, "fenec", None).unwrap();
    let mut other = Client::connect(h.port, "fenec", None).unwrap();
    c.simple("create collection t (name text)");

    c.simple("BEGIN");
    c.simple("put t {name: \"a\"}");
    c.simple("SAVEPOINT s");
    c.simple("put t {name: \"b\"}");
    let r = c.simple("put t [{name: \"c\"}, {name: 1}]");
    assert_eq!(find(&r, b'E').unwrap().sqlstate().unwrap(), "42804");
    assert_eq!(status(&r), b'E');
    for q in ["get t", "SAVEPOINT t", "RELEASE s"] {
        let r = c.simple(q);
        assert_eq!(find(&r, b'E').unwrap().sqlstate().unwrap(), "25P02", "{q}");
    }
    other.send("get t count");
    assert!(
        !answers_within(&mut other, 300),
        "the failed transaction let go of what its savepoint keeps"
    );
    // A savepoint it does not have leaves it failed.
    let r = c.simple("ROLLBACK TO nosuch");
    assert_eq!(find(&r, b'E').unwrap().sqlstate().unwrap(), "3B001");
    assert_eq!(status(&r), b'E');
    let r = c.simple("ROLLBACK TO s");
    assert_eq!(find(&r, b'C').unwrap().tag_text(), "ROLLBACK");
    assert_eq!(status(&r), b'T');
    assert_eq!(names(&mut c), ["a"]);
    c.simple("put t {name: \"d\"}");
    c.simple("COMMIT");
    let r = other.until_ready();
    assert_eq!(find(&r, b'D').unwrap().cells(), [Some("2".to_string())]);
    assert_eq!(names(&mut other), ["a", "d"]);

    // Failed again with nothing to go back to, it takes only its end, and
    // a COMMIT puts it back.
    c.simple("BEGIN");
    c.simple("SAVEPOINT s");
    c.simple("put t {name: \"e\"}");
    c.simple("SAVEPOINT u");
    c.simple("put t {name: 1}");
    c.simple("ROLLBACK TO u");
    c.simple("put t {name: 1}");
    let r = c.simple("COMMIT");
    assert_eq!(find(&r, b'C').unwrap().tag_text(), "ROLLBACK");
    assert_eq!(status(&r), b'I');
    assert_eq!(names(&mut other), ["a", "d"]);
}

/// A savepoint before a transaction's first write holds nothing: a failure
/// after it lets the lock go at once, and taken back to, it lets the lock go
/// and the transaction reads what others commit again.
#[test]
fn a_savepoint_before_the_first_write_lets_the_lock_go() {
    let h = trust_server();
    let mut c = Client::connect(h.port, "fenec", None).unwrap();
    let mut other = Client::connect(h.port, "fenec", None).unwrap();
    c.simple("create collection t (name text)");

    c.simple("BEGIN");
    c.simple("SAVEPOINT s");
    c.simple("put t {name: \"a\"}");
    other.send("put t {name: \"other\"}");
    assert!(!answers_within(&mut other, 300));
    c.simple("ROLLBACK TO s");
    assert_eq!(
        find(&other.until_ready(), b'C').unwrap().tag_text(),
        "INSERT 0 1"
    );
    assert_eq!(names(&mut c), ["other"]);

    c.simple("put t {name: \"b\"}");
    let r = c.simple("put t {name: 1}");
    assert_eq!(status(&r), b'E');
    assert_eq!(
        names(&mut other),
        ["other"],
        "the lock stayed with a failure nothing needs kept"
    );
    let r = c.simple("ROLLBACK TO s");
    assert_eq!(status(&r), b'T');
    c.simple("put t {name: \"c\"}");
    c.simple("COMMIT");
    assert_eq!(names(&mut other), ["other", "c"]);
}

/// Savepoints outside a transaction, and the names PostgreSQL resolves: the
/// newest of a name, until it is released.
#[test]
fn savepoints_are_named_and_refused_as_in_postgresql() {
    let h = trust_server();
    let mut c = Client::connect(h.port, "fenec", None).unwrap();
    c.simple("create collection t (name text)");

    for q in ["SAVEPOINT s", "RELEASE s", "ROLLBACK TO s"] {
        let r = c.simple(q);
        assert_eq!(find(&r, b'E').unwrap().sqlstate().unwrap(), "25P01", "{q}");
        assert_eq!(status(&r), b'I');
    }

    c.simple("BEGIN");
    c.simple("SAVEPOINT a");
    c.simple("put t {name: \"x\"}");
    c.simple("SAVEPOINT A");
    c.simple("put t {name: \"y\"}");
    // Unquoted names fold to lower case: this is the second `a`.
    c.simple("ROLLBACK TO a");
    assert_eq!(names(&mut c), ["x"]);
    c.simple("RELEASE a");
    c.simple("ROLLBACK TO \"a\"");
    assert!(names(&mut c).is_empty());
    let r = c.simple("RELEASE \"A\"");
    assert_eq!(find(&r, b'E').unwrap().sqlstate().unwrap(), "3B001");
    c.simple("ROLLBACK");

    // A schema change since a savepoint is put back with the writes.
    c.simple("BEGIN");
    c.simple("SAVEPOINT s");
    c.simple("create collection u (x int)");
    c.simple("SAVEPOINT after");
    c.simple("put u {x: 1}");
    let r = c.simple("ROLLBACK TO after");
    assert_eq!(status(&r), b'T');
    let r = c.simple("get u count");
    assert_eq!(find(&r, b'D').unwrap().cells(), [Some("0".to_string())]);
    let r = c.simple("ROLLBACK TO s");
    assert_eq!(status(&r), b'T');
    c.simple("COMMIT");
    let r = c.simple("get u");
    assert_eq!(find(&r, b'E').unwrap().sqlstate().unwrap(), "42P01");

    // Two-phase commit stays refused rather than read as what it begins
    // with: `COMMIT PREPARED 'x'` committed the transaction open.
    for q in ["COMMIT PREPARED 'x'", "PREPARE TRANSACTION 'x'"] {
        let r = c.simple(q);
        assert_eq!(find(&r, b'E').unwrap().sqlstate().unwrap(), "0A000", "{q}");
    }
    assert!(names(&mut c).is_empty());
}

/// A serializable transaction keeps the lock through a failure and a
/// ROLLBACK TO, even to a savepoint before its first write: what it read
/// before the savepoint must not change under it.
#[test]
fn a_serializable_transaction_keeps_the_lock_through_a_rollback_to() {
    let h = trust_server();
    let mut c = Client::connect(h.port, "fenec", None).unwrap();
    let mut other = Client::connect(h.port, "fenec", None).unwrap();
    c.simple("create collection t (name text)");

    c.simple("BEGIN ISOLATION LEVEL SERIALIZABLE");
    c.simple("SAVEPOINT s");
    assert!(names(&mut c).is_empty());
    c.simple("put t {name: \"a\"}");
    c.simple("put t {name: 1}");
    other.send("put t {name: \"late\"}");
    assert!(!answers_within(&mut other, 300));
    let r = c.simple("ROLLBACK TO s");
    assert_eq!(status(&r), b'T');
    assert!(!answers_within(&mut other, 300));
    assert!(names(&mut c).is_empty());
    c.simple("COMMIT");
    assert_eq!(
        find(&other.until_ready(), b'C').unwrap().tag_text(),
        "INSERT 0 1"
    );
    assert_eq!(names(&mut c), ["late"]);
}

/// A text holding transaction control runs a statement at a time, as
/// PostgreSQL runs a simple query: each statement answered, a BEGIN or a
/// COMMIT where it stands, the statements outside a transaction one
/// implicit block the text's end lands, and nothing after the first error.
#[test]
fn a_text_holding_transaction_control_runs_a_statement_at_a_time() {
    let h = trust_server();
    let mut c = Client::connect(h.port, "fenec", None).unwrap();
    let mut other = Client::connect(h.port, "fenec", None).unwrap();
    c.simple("create collection t (name text)");
    let tags_of = |r: &[Msg]| -> Vec<String> {
        r.iter()
            .filter(|m| m.tag == b'C')
            .map(|m| m.tag_text())
            .collect()
    };

    // A semicolon in a string is no end of a statement.
    let r = c.simple("BEGIN; put t {name: \"a;1\"}; put t {name: 'b'}; COMMIT");
    assert_eq!(tags_of(&r), ["BEGIN", "INSERT 0 1", "INSERT 0 1", "COMMIT"]);
    assert_eq!(status(&r), b'I');
    assert_eq!(names(&mut other), ["a;1", "b"]);
    let r = c.simple("BEGIN; put t {name: \"gone\"}; ROLLBACK");
    assert_eq!(tags_of(&r), ["BEGIN", "INSERT 0 1", "ROLLBACK"]);
    assert_eq!(names(&mut other), ["a;1", "b"]);

    // After a COMMIT the rest is an implicit block, put back at an error.
    let r = c.simple("BEGIN; put t {name: \"c\"}; COMMIT; put t {name: \"gone\"}; put t {name: 1}");
    assert_eq!(tags_of(&r), ["BEGIN", "INSERT 0 1", "COMMIT", "INSERT 0 1"]);
    assert_eq!(find(&r, b'E').unwrap().sqlstate().unwrap(), "42804");
    assert_eq!(status(&r), b'I');
    assert_eq!(names(&mut other), ["a;1", "b", "c"]);

    // A BEGIN takes the implicit block's writes into the transaction it
    // opens, which the text leaves open.
    let r = c.simple("put t {name: \"d\"}; BEGIN; put t {name: \"e\"}");
    assert_eq!(tags_of(&r), ["INSERT 0 1", "BEGIN", "INSERT 0 1"]);
    assert_eq!(status(&r), b'T');
    c.simple("ROLLBACK");
    assert_eq!(names(&mut other), ["a;1", "b", "c"]);

    // A savepoint in a transaction the text opened; none outside one.
    let r = c.simple(
        "BEGIN; put t {name: \"d\"}; SAVEPOINT s; put t {name: \"gone\"}; ROLLBACK TO s; COMMIT",
    );
    assert_eq!(
        tags_of(&r),
        [
            "BEGIN",
            "INSERT 0 1",
            "SAVEPOINT",
            "INSERT 0 1",
            "ROLLBACK",
            "COMMIT"
        ]
    );
    assert_eq!(names(&mut other), ["a;1", "b", "c", "d"]);
    let r = c.simple("put t {name: \"gone\"}; SAVEPOINT s; put t {name: \"gone\"}");
    assert_eq!(find(&r, b'E').unwrap().sqlstate().unwrap(), "25P01");
    assert_eq!(names(&mut other), ["a;1", "b", "c", "d"]);

    // A COMMIT with no transaction warns, and closes the implicit block.
    let r = c.simple("put t {name: \"e\"}; COMMIT; SET application_name = 'x'");
    assert_eq!(tags_of(&r), ["INSERT 0 1", "COMMIT", "SET"]);
    assert_eq!(find(&r, b'N').unwrap().sqlstate().unwrap(), "25P01");
    assert_eq!(names(&mut other), ["a;1", "b", "c", "d", "e"]);

    // The first error ends the text, a transaction it opened left failed.
    let r = c.simple("BEGIN; get nosuch; ROLLBACK");
    assert_eq!(tags_of(&r), ["BEGIN"]);
    assert_eq!(status(&r), b'E');
    c.simple("ROLLBACK");

    // A statement it cannot read runs none of it.
    let r = c.simple("BEGIN; put t {name: \"gone\"}; COMMIT; gett t");
    assert_eq!(find(&r, b'E').unwrap().sqlstate().unwrap(), "42601");
    assert!(tags_of(&r).is_empty());
    assert_eq!(status(&r), b'I');
    assert_eq!(names(&mut other), ["a;1", "b", "c", "d", "e"]);

    // The extended protocol takes one command, as PostgreSQL's does.
    for q in ["BEGIN; put t {name: $1}", "BEGIN ; put t {name: $1}"] {
        let r = c.extended(q, &["gone"], false);
        assert_eq!(find(&r, b'E').unwrap().sqlstate().unwrap(), "42601", "{q}");
    }
    assert_eq!(names(&mut other), ["a;1", "b", "c", "d", "e"]);
}

/// `COMMIT AND CHAIN` lands the transaction and begins the next.
#[test]
fn commit_and_chain_begins_the_next_transaction() {
    let h = trust_server();
    let mut c = Client::connect(h.port, "fenec", None).unwrap();
    c.simple("create collection t (name text)");

    c.simple("BEGIN");
    c.simple("put t {name: \"a\"}");
    let r = c.simple("COMMIT AND CHAIN");
    assert_eq!(find(&r, b'C').unwrap().tag_text(), "COMMIT");
    assert_eq!(status(&r), b'T');
    c.simple("put t {name: \"b\"}");
    c.simple("ROLLBACK");
    assert_eq!(names(&mut c), ["a"]);
}

/// A transaction that has written holds the database: one whose client
/// goes silent is put back after `idle_in_transaction`, and its session
/// closed with the reason; one whose client goes away is put back at once.
#[test]
fn an_idle_or_abandoned_transaction_is_put_back() {
    let h = start(
        Config {
            idle_in_transaction: Some(Duration::from_millis(300)),
            ..Config::default()
        },
        bare_db(),
    );
    let mut c = Client::connect(h.port, "fenec", None).unwrap();
    c.simple("create collection t (name text)");
    c.simple("BEGIN");
    c.simple("put t {name: \"idle\"}");
    let started = Instant::now();
    let mut other = Client::connect(h.port, "fenec", None).unwrap();
    assert!(names(&mut other).is_empty(), "the idle transaction landed");
    assert!(started.elapsed() >= Duration::from_millis(250));
    let m = c.read_msg().expect("the server closed without saying why");
    assert_eq!(m.tag, b'E');
    assert_eq!(m.sqlstate().as_deref(), Some("25P03"), "{:?}", m.message());

    let mut gone = Client::connect(h.port, "fenec", None).unwrap();
    gone.simple("BEGIN");
    gone.simple("put t {name: \"gone\"}");
    drop(gone);
    assert!(
        names(&mut other).is_empty(),
        "the abandoned transaction landed"
    );
}

/// `LISTEN` would leave a client waiting for notifications that never come;
/// `UNLISTEN` is true as it is.
#[test]
fn listen_and_notify_are_refused() {
    let h = trust_server();
    let mut c = Client::connect(h.port, "fenec", None).unwrap();
    for q in ["LISTEN jobs", "NOTIFY jobs, 'ready'"] {
        let r = c.simple(q);
        let e = find(&r, b'E').unwrap_or_else(|| panic!("`{q}` must be refused"));
        assert_eq!(e.sqlstate().unwrap(), "0A000");
        assert!(e.message().unwrap().contains("/changes"));
    }
    let r = c.simple("UNLISTEN *");
    assert_eq!(find(&r, b'C').unwrap().tag_text(), "UNLISTEN");
}

// ------------------------------------------------------------ storage errors

/// A sink whose `sync` fails: what a dying disk or a full volume looks like
/// from above.
struct SyncFails;

impl fenec_core::engine::Sink for SyncFails {
    fn append(&mut self, _bytes: &[u8]) -> fenec_core::error::Result<()> {
        Ok(())
    }
    fn rewrite(&mut self, _bytes: &[u8]) -> fenec_core::error::Result<()> {
        Ok(())
    }
    fn sync(&mut self) -> fenec_core::error::Result<()> {
        Err(fenec_core::error::Error::Io("input/output error".into()))
    }
}

/// Under `--sync always` a write the disk refused is reported as failed, and
/// every write after it is refused until the file is reopened; reads go on.
#[test]
fn sync_failure_is_reported_and_stops_writes() {
    let mut db = Database::with_sink(Box::new(SyncFails));
    db.install_plugin(&PgPlugin).unwrap();
    let h = start(
        Config {
            sync: SyncPolicy::Always,
            ..Config::default()
        },
        db,
    );
    let mut c = Client::connect(h.port, "fenec", None).unwrap();

    let r = c.simple("create collection t (name text)");
    let e = find(&r, b'E').expect("the failed sync must reach the client");
    assert_eq!(e.sqlstate().unwrap(), "58030");
    assert!(
        find(&r, b'C').is_none(),
        "no CommandComplete for a write the disk refused"
    );

    let r = c.simple("put t {name: \"a\"}");
    let e = find(&r, b'E').expect("later writes are refused");
    assert_eq!(e.sqlstate().unwrap(), "58030");
    assert!(e.message().unwrap().contains("refused"));

    // Reads still answer, from memory.
    let r = c.simple("collections");
    assert!(find(&r, b'E').is_none());
    assert!(h.db.read().unwrap().failure().is_some());
}

/// A sink whose fsync, handed out by `flush`, takes `delay` and fails when
/// `fail` is set -- the half of a sync that runs outside the lock. It keeps
/// `FileSink`'s protocol: an fsync covers every byte appended before it
/// starts, and one that finds its bytes covered does not run. It counts the
/// fsyncs it ran.
#[derive(Clone)]
struct SlowDisk {
    delay: Duration,
    fail: bool,
    fsyncs: Arc<AtomicUsize>,
    appended: Arc<AtomicU64>,
    /// Bytes on disk; held through an fsync, as the file is.
    synced: Arc<Mutex<u64>>,
}

impl fenec_core::engine::Sink for SlowDisk {
    fn append(&mut self, bytes: &[u8]) -> fenec_core::error::Result<()> {
        self.appended
            .fetch_add(bytes.len() as u64, Ordering::SeqCst);
        Ok(())
    }
    fn rewrite(&mut self, _bytes: &[u8]) -> fenec_core::error::Result<()> {
        Ok(())
    }
    fn flush(&mut self) -> fenec_core::error::Result<Option<fenec_core::engine::Durability>> {
        let disk = self.clone();
        let upto = self.appended.load(Ordering::SeqCst);
        Ok(Some(Box::new(move || {
            let mut synced = disk.synced.lock().unwrap();
            if *synced >= upto {
                return Ok(());
            }
            let covers = disk.appended.load(Ordering::SeqCst);
            std::thread::sleep(disk.delay);
            disk.fsyncs.fetch_add(1, Ordering::SeqCst);
            if disk.fail {
                return Err(fenec_core::error::Error::Io("input/output error".into()));
            }
            *synced = covers;
            Ok(())
        })))
    }
}

fn slow_disk_server(delay: Duration, fail: bool) -> (Harness, Arc<AtomicUsize>) {
    let fsyncs = Arc::new(AtomicUsize::new(0));
    let mut db = Database::with_sink(Box::new(SlowDisk {
        delay,
        fail,
        fsyncs: Arc::clone(&fsyncs),
        appended: Arc::new(AtomicU64::new(0)),
        synced: Arc::new(Mutex::new(0)),
    }));
    db.install_plugin(&PgPlugin).unwrap();
    db.execute(&fenec_ql::parse_one("create collection t (n int)").unwrap())
        .unwrap();
    let h = start(
        Config {
            sync: SyncPolicy::Always,
            ..Config::default()
        },
        db,
    );
    (h, fsyncs)
}

/// Under `--sync always` the fsync runs without the lock: a read arriving
/// while a write waits on the disk answers at once, and writes arriving
/// together share fsyncs instead of queueing for one each.
#[test]
fn a_durable_write_does_not_hold_the_readers_or_the_other_writers() {
    let delay = Duration::from_millis(200);
    let (h, fsyncs) = slow_disk_server(delay, false);

    let port = h.port;
    let writer = std::thread::spawn(move || {
        let mut c = Client::connect(port, "fenec", None).unwrap();
        let r = c.simple("put t {n: 1}");
        assert_eq!(find(&r, b'C').unwrap().tag_text(), "INSERT 0 1");
    });
    std::thread::sleep(Duration::from_millis(50));
    let mut reader = Client::connect(h.port, "fenec", None).unwrap();
    let t = Instant::now();
    let r = reader.simple("get t count");
    assert!(find(&r, b'D').is_some());
    let read = t.elapsed();
    writer.join().unwrap();
    // The write under way was still inside its 200 ms fsync.
    assert!(
        read < Duration::from_millis(100),
        "the read waited {read:?}"
    );

    // Eight writers at once: with an fsync each under the lock they took
    // 8 x 200 ms. Sharing, they take two or three fsyncs.
    let before = fsyncs.load(Ordering::SeqCst);
    let t = Instant::now();
    let writers: Vec<_> = (0..8)
        .map(|i| {
            std::thread::spawn(move || {
                let mut c = Client::connect(port, "fenec", None).unwrap();
                let r = c.simple(&format!("put t {{n: {i}}}"));
                assert_eq!(find(&r, b'C').unwrap().tag_text(), "INSERT 0 1");
            })
        })
        .collect();
    for w in writers {
        w.join().unwrap();
    }
    let took = t.elapsed();
    let ran = fsyncs.load(Ordering::SeqCst) - before;
    assert!(ran <= 4, "{ran} fsyncs for 8 writes");
    assert!(
        took < Duration::from_millis(1000),
        "8 durable writes took {took:?}"
    );
}

/// The fsync that fails outside the lock: the write waiting on it is told,
/// its answer taken back; the engine refuses every write after it, as when
/// the sync ran under the lock; reads go on.
#[test]
fn a_failed_fsync_outside_the_lock_is_reported_and_stops_writes() {
    let (h, _) = slow_disk_server(Duration::from_millis(1), true);
    let mut c = Client::connect(h.port, "fenec", None).unwrap();

    let r = c.simple("put t {n: 1}");
    let e = find(&r, b'E').expect("the failed fsync must reach the client");
    assert_eq!(e.sqlstate().unwrap(), "58030");
    assert!(find(&r, b'C').is_none(), "the answer was taken back");
    assert_eq!(status(&r), b'I', "and the session is still ready");

    let r = c.simple("put t {n: 2}");
    let e = find(&r, b'E').expect("later writes are refused");
    assert_eq!(e.sqlstate().unwrap(), "58030");
    assert!(e.message().unwrap().contains("refused"));

    let r = c.simple("get t count");
    assert!(find(&r, b'E').is_none());
    assert!(h.db.read().unwrap().failure().is_some());
}

/// `explain` over the wire: one `plan` text column, reported by Describe
/// before the query runs, a row a step, and the `EXPLAIN` tag PostgreSQL
/// sends for a plan.
#[test]
fn explain_is_a_text_column_tagged_explain() {
    let h = trust_server();
    let mut c = Client::connect(h.port, "fenec", None).unwrap();
    c.simple("create collection t (name text, year int @sorted)");
    c.simple(r#"put t [{name: "a", year: 2024}, {name: "b", year: 2023}]"#);

    let r = c.simple("explain get t where year >= 2024");
    assert_eq!(
        find(&r, b'T').unwrap().columns(),
        vec![("plan".to_string(), 25)]
    );
    let steps: Vec<Option<String>> = r
        .iter()
        .filter(|m| m.tag == b'D')
        .map(|m| m.cells()[0].clone())
        .collect();
    assert_eq!(
        steps,
        vec![
            Some("filter: the ordered index on year, 1 rows, which is the answer".to_string()),
            Some("rows: 1".to_string()),
        ]
    );
    assert_eq!(find(&r, b'C').unwrap().tag_text(), "EXPLAIN");

    let r = c.extended("explain get t where year >= $1", &["2023"], true);
    assert_eq!(find(&r, b't').unwrap().param_oids().len(), 1);
    assert_eq!(
        find(&r, b'T').unwrap().columns(),
        vec![("plan".to_string(), 25)]
    );
    assert_eq!(find(&r, b'C').unwrap().tag_text(), "EXPLAIN");
}

/// psql's `\d`, and JDBC's column lookup with its parameter bound: the
/// catalog answers with the collections, their fields and their indexes
/// rather than the empty result every catalog query once got.
#[test]
fn the_catalog_describes_the_collections() {
    let h = trust_server();
    let mut c = Client::connect(h.port, "fenec", None).unwrap();
    c.simple(
        "create collection docs (title text @hash, n int @sorted, embed vector<3> @hnsw(cosine))",
    );
    c.simple("create collection notes (x float)");

    let rows = |msgs: &[Msg]| -> Vec<Vec<Option<String>>> {
        msgs.iter()
            .filter(|m| m.tag == b'D')
            .map(|m| m.cells())
            .collect()
    };
    let r = c.simple(
        "SELECT n.nspname as \"Schema\", c.relname as \"Name\",
           CASE c.relkind WHEN 'r' THEN 'table' WHEN 'i' THEN 'index' END as \"Type\"
         FROM pg_catalog.pg_class c
              LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
         WHERE c.relkind IN ('r','p','v','m','S','f','')
               AND n.nspname <> 'pg_catalog' AND n.nspname !~ '^pg_toast'
           AND pg_catalog.pg_table_is_visible(c.oid)
         ORDER BY 1,2;",
    );
    let named = |s: &str| Some(s.to_string());
    assert_eq!(
        rows(&r),
        [
            [named("public"), named("docs"), named("table")],
            [named("public"), named("notes"), named("table")]
        ]
    );
    assert_eq!(find(&r, b'C').unwrap().tag_text(), "SELECT 2");

    // Over the extended protocol, with the schema's oid bound as JDBC binds
    // it: the columns come typed from Describe, the id first.
    let r = c.extended(
        "SELECT a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod), a.attnotnull, a.attnum
         FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_class c ON a.attrelid = c.oid
         WHERE c.relnamespace = $1 AND c.relname = $2 AND a.attnum > 0 ORDER BY a.attnum",
        &["2200", "docs"],
        true,
    );
    assert_eq!(find(&r, b't').unwrap().param_oids().len(), 2);
    assert_eq!(
        find(&r, b'T').unwrap().columns(),
        vec![
            ("attname".to_string(), 19),
            ("format_type".to_string(), 25),
            ("attnotnull".to_string(), 16),
            ("attnum".to_string(), 21)
        ]
    );
    let described: Vec<(Option<String>, Option<String>)> = rows(&r)
        .into_iter()
        .map(|r| (r[0].clone(), r[1].clone()))
        .collect();
    assert_eq!(
        described,
        [
            (named("id"), named("bigint")),
            (named("title"), named("text")),
            (named("n"), named("bigint")),
            (named("embed"), named("vector(3)"))
        ]
    );

    // An index a field carries is an index with its own access method.
    let r = c.simple(
        "SELECT c.relname, am.amname FROM pg_catalog.pg_index i
         JOIN pg_catalog.pg_class c ON c.oid = i.indexrelid
         JOIN pg_catalog.pg_am am ON am.oid = c.relam
         WHERE i.indrelid = 'docs'::regclass ORDER BY 1",
    );
    let indexes: Vec<String> = rows(&r)
        .into_iter()
        .map(|r| format!("{} {}", r[0].clone().unwrap(), r[1].clone().unwrap()))
        .collect();
    assert_eq!(
        indexes,
        [
            "docs_embed_hnsw hnsw",
            "docs_n_sorted btree",
            "docs_pkey btree",
            "docs_title_hash hash"
        ]
    );

    // A query the catalog cannot read still answers, empty, as before.
    let r = c.simple("WITH x AS (SELECT 1) SELECT * FROM pg_catalog.pg_class, x");
    assert!(find(&r, b'E').is_none(), "{r:?}");
    assert!(rows(&r).is_empty());
}

/// A `sparse<N>` field travels as pgvector's `sparsevec` does -- the text
/// form `{1:0.5,3:0.25}/N`, indices from 1 -- both ways, as a parameter to
/// `near` too, and the catalog calls the type and its index by name.
#[test]
fn sparse_vectors_travel_in_pgvectors_text_form() {
    let h = trust_server();
    let mut c = Client::connect(h.port, "fenec", None).unwrap();
    c.simple("create collection docs (title text, s sparse<30522> @inverted)");
    let r = c.extended(
        "put docs {title: $1, s: $2}",
        &["one", "{1:0.5,3:0.25}/30522"],
        false,
    );
    assert!(
        find(&r, b'E').is_none(),
        "{:?}",
        find(&r, b'E').map(|m| m.cells())
    );
    c.simple(r#"put docs {title: "two", s: "{3:2}/30522"}"#);

    let r = c.simple("get docs select title, s");
    let cells: Vec<Vec<Option<String>>> = r
        .iter()
        .filter(|m| m.tag == b'D')
        .map(|m| m.cells())
        .collect();
    assert_eq!(cells[0][1].as_deref(), Some("{1:0.5,3:0.25}/30522"));

    let r = c.extended(
        "get docs select title near s $1 limit 2",
        &["{3:1}/30522"],
        false,
    );
    let hits: Vec<Vec<Option<String>>> = r
        .iter()
        .filter(|m| m.tag == b'D')
        .map(|m| m.cells())
        .collect();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0][0].as_deref(), Some("two"));
    assert_eq!(hits[0][1].as_deref(), Some("2"));
    assert_eq!(hits[1][1].as_deref(), Some("0.25"));

    let r = c.simple(
        "SELECT pg_catalog.format_type(a.atttypid, a.atttypmod)
         FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_class c ON a.attrelid = c.oid
         WHERE c.relname = 'docs' AND a.attname = 's'",
    );
    assert_eq!(
        find(&r, b'D').unwrap().cells()[0].as_deref(),
        Some("sparsevec(30522)")
    );
    let r = c.simple(
        "SELECT am.amname FROM pg_catalog.pg_index i
         JOIN pg_catalog.pg_class c ON c.oid = i.indexrelid
         JOIN pg_catalog.pg_am am ON am.oid = c.relam WHERE c.relname = 'docs_s_inverted'",
    );
    assert_eq!(
        find(&r, b'D').unwrap().cells()[0].as_deref(),
        Some("inverted")
    );
}
