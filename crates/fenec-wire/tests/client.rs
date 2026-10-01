//! The client against a stand-in for PostgreSQL.
//!
//! The client was held to fenecdb's own pg server, whose SCRAM half was
//! written by hand too, so each checked the other. That server is gone; the
//! stand-in here keeps its half of SCRAM (RFC 5802 and 7677, checked
//! against RFC 7677's example below), a cleartext password, and the
//! answers the client reads: rows, an error, a COPY stream whose `CopyData`
//! frames do not align with its lines. A live PostgreSQL is
//! `make import-test`'s.

use fenec_http::crypto::*;
use fenec_wire::client::{Client, Url};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

// ------------------------------------------------------------- framing

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

fn read_msg(s: &mut TcpStream) -> Option<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    s.read_exact(&mut head).ok()?;
    let len = i32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize - 4;
    let mut body = vec![0u8; len];
    s.read_exact(&mut body).ok()?;
    Some((head[0], body))
}

fn error(code: &str, message: &str) -> Vec<u8> {
    let mut b = Vec::new();
    for (k, v) in [(b'S', "ERROR"), (b'C', code), (b'M', message)] {
        b.push(k);
        cstr(&mut b, v);
    }
    b.push(0);
    framed(b'E', &b)
}

// ---------------------------------------------------- SCRAM, server half

const ITERS: u32 = 4096;

struct Verifier {
    salt: Vec<u8>,
    stored_key: [u8; SHA256_LEN],
    server_key: [u8; SHA256_LEN],
}

impl Verifier {
    fn new(password: &str, salt: Vec<u8>) -> Verifier {
        let salted = pbkdf2_sha256(password.as_bytes(), &salt, ITERS);
        let client_key = hmac_sha256(&salted, b"Client Key");
        Verifier {
            salt,
            stored_key: sha256(&client_key),
            server_key: hmac_sha256(&salted, b"Server Key"),
        }
    }
}

fn attr(msg: &str, key: char) -> Option<&str> {
    msg.split(',').find_map(|kv| {
        let mut it = kv.splitn(2, '=');
        let k = it.next()?;
        (k.len() == 1 && k.starts_with(key))
            .then(|| it.next())
            .flatten()
    })
}

/// One handshake: `client-first` -> `server-first`, then `client-final` ->
/// `server-final`, or the refusal of a wrong proof.
struct Exchange<'a> {
    v: &'a Verifier,
    client_first_bare: String,
    server_first: String,
    nonce: String,
}

impl<'a> Exchange<'a> {
    fn client_first(v: &'a Verifier, msg: &str, snonce: &str) -> (Exchange<'a>, String) {
        let bare = msg.splitn(3, ',').nth(2).expect("a gs2 header").to_string();
        let nonce = format!("{}{snonce}", attr(&bare, 'r').expect("a client nonce"));
        let server_first = format!("r={nonce},s={},i={ITERS}", b64_encode(&v.salt));
        let ex = Exchange {
            v,
            client_first_bare: bare,
            server_first: server_first.clone(),
            nonce,
        };
        (ex, server_first)
    }

    fn client_final(&self, msg: &str) -> Result<String, String> {
        let (without_proof, _) = msg.rsplit_once(",p=").ok_or("no proof")?;
        if attr(msg, 'r') != Some(self.nonce.as_str()) {
            return Err("the nonce does not match".into());
        }
        let proof = attr(msg, 'p').and_then(b64_decode).ok_or("a bad proof")?;
        let auth = format!(
            "{},{},{without_proof}",
            self.client_first_bare, self.server_first
        );
        let client_sig = hmac_sha256(&self.v.stored_key, auth.as_bytes());
        let mut client_key = [0u8; SHA256_LEN];
        for i in 0..SHA256_LEN {
            client_key[i] = proof.get(i).copied().unwrap_or(0) ^ client_sig[i];
        }
        if !ct_eq(&sha256(&client_key), &self.v.stored_key) {
            return Err("password authentication failed".into());
        }
        let server_sig = hmac_sha256(&self.v.server_key, auth.as_bytes());
        Ok(format!("v={}", b64_encode(&server_sig)))
    }
}

/// RFC 7677's example: the server half gives the values the RFC does, so
/// the client checked against it is checked against SCRAM itself.
#[test]
fn the_server_half_answers_as_rfc_7677_does() {
    let salt = b64_decode("W22ZaJ0SNY7soEsUEjb6gQ==").unwrap();
    let v = Verifier::new("pencil", salt);
    let (ex, server_first) = Exchange::client_first(
        &v,
        "n,,n=user,r=rOprNGfwEbeRWgbNEkqO",
        "%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0",
    );
    assert_eq!(
        server_first,
        "r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096"
    );
    let server_final = ex
        .client_final(
            "c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,\
             p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=",
        )
        .unwrap();
    assert_eq!(
        server_final,
        "v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4="
    );
}

// ------------------------------------------------------------ the server

enum Auth {
    Trust,
    Cleartext(&'static str),
    Scram(&'static str),
}

/// A server that authenticates as `auth` says, then answers each query:
/// one naming `no_such` with an error, any other with two rows of `name`
/// and `score` -- a thread a connection, every query on it.
fn server(auth: Auth) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let auth = std::sync::Arc::new(auth);
    std::thread::spawn(move || {
        for s in listener.incoming() {
            let auth = std::sync::Arc::clone(&auth);
            std::thread::spawn(move || session(s.unwrap(), &auth));
        }
    });
    port
}

fn session(mut s: TcpStream, auth: &Auth) {
    // StartupMessage: its length, then a body the stand-in ignores.
    let mut len = [0u8; 4];
    s.read_exact(&mut len).unwrap();
    let mut body = vec![0u8; i32::from_be_bytes(len) as usize - 4];
    s.read_exact(&mut body).unwrap();
    if !authenticate(auth, &mut s) {
        return;
    }
    let mut out = framed(b'R', &0i32.to_be_bytes());
    let mut ps = Vec::new();
    cstr(&mut ps, "server_version");
    cstr(&mut ps, "17.0");
    out.extend_from_slice(&framed(b'S', &ps));
    out.extend_from_slice(&framed(b'Z', b"I"));
    s.write_all(&out).unwrap();
    while let Some((tag, q)) = read_msg(&mut s) {
        if tag != b'Q' {
            break;
        }
        let q = String::from_utf8_lossy(&q);
        let mut out = Vec::new();
        if q.contains("no_such") {
            out.extend_from_slice(&error("42P01", "no such table"));
        } else {
            let mut d = 2i16.to_be_bytes().to_vec();
            for (name, oid) in [("name", 25i32), ("score", 20)] {
                cstr(&mut d, name);
                d.extend_from_slice(&0i32.to_be_bytes());
                d.extend_from_slice(&0i16.to_be_bytes());
                d.extend_from_slice(&oid.to_be_bytes());
                d.extend_from_slice(&(-1i16).to_be_bytes());
                d.extend_from_slice(&(-1i32).to_be_bytes());
                d.extend_from_slice(&0i16.to_be_bytes());
            }
            out.extend_from_slice(&framed(b'T', &d));
            for (name, score) in [("one", "1"), ("two", "2")] {
                let mut r = 2i16.to_be_bytes().to_vec();
                for cell in [name, score] {
                    r.extend_from_slice(&(cell.len() as i32).to_be_bytes());
                    r.extend_from_slice(cell.as_bytes());
                }
                out.extend_from_slice(&framed(b'D', &r));
            }
            out.extend_from_slice(&framed(b'C', b"SELECT 2\0"));
        }
        out.extend_from_slice(&framed(b'Z', b"I"));
        s.write_all(&out).unwrap();
    }
}

/// The password exchange; `false`, the refusal sent, for a wrong one.
fn authenticate(auth: &Auth, s: &mut TcpStream) -> bool {
    let refuse = |s: &mut TcpStream| {
        let _ = s.write_all(&error("28P01", "password authentication failed"));
        false
    };
    match auth {
        Auth::Trust => true,
        Auth::Cleartext(expected) => {
            s.write_all(&framed(b'R', &3i32.to_be_bytes())).unwrap();
            let Some((b'p', body)) = read_msg(s) else {
                return false;
            };
            let got = body.strip_suffix(&[0]).unwrap_or(&body);
            ct_eq(got, expected.as_bytes()) || refuse(s)
        }
        Auth::Scram(password) => {
            let v = Verifier::new(password, random_bytes(16));
            let mut b = 10i32.to_be_bytes().to_vec();
            cstr(&mut b, "SCRAM-SHA-256");
            b.push(0);
            s.write_all(&framed(b'R', &b)).unwrap();
            // SASLInitialResponse: the mechanism, a length, the first message.
            let Some((b'p', body)) = read_msg(s) else {
                return false;
            };
            let at = body.iter().position(|&c| c == 0).unwrap() + 1 + 4;
            let first = String::from_utf8_lossy(&body[at..]).into_owned();
            let (ex, server_first) = Exchange::client_first(&v, &first, &nonce(18));
            let mut b = 11i32.to_be_bytes().to_vec();
            b.extend_from_slice(server_first.as_bytes());
            s.write_all(&framed(b'R', &b)).unwrap();
            let Some((b'p', body)) = read_msg(s) else {
                return false;
            };
            match ex.client_final(&String::from_utf8_lossy(&body)) {
                Ok(server_final) => {
                    let mut b = 12i32.to_be_bytes().to_vec();
                    b.extend_from_slice(server_final.as_bytes());
                    s.write_all(&framed(b'R', &b)).unwrap();
                    true
                }
                Err(_) => refuse(s),
            }
        }
    }
}

fn url(port: u16, password: Option<&str>) -> Url {
    Url {
        user: "fenec".into(),
        password: password.map(String::from),
        host: "127.0.0.1".into(),
        port,
        database: "fenec".into(),
    }
}

#[test]
fn connects_without_authentication() {
    let port = server(Auth::Trust);
    let mut c = Client::connect(&url(port, None)).unwrap();
    assert!(!c.server_version.is_empty(), "ParameterStatus was not read");

    let r = c.query("select name, score from t").unwrap();
    let names: Vec<&str> = r.columns.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, vec!["name", "score"]);
    assert_eq!(r.rows.len(), 2);
    assert_eq!(r.rows[0][0].as_deref(), Some("one"));
    assert_eq!(r.rows[1][1].as_deref(), Some("2"));
}

/// The client half of SCRAM against the server half above.
#[test]
fn scram_handshake_succeeds_and_rejects() {
    let port = server(Auth::Scram("right-password"));
    let c = Client::connect(&url(port, Some("right-password")));
    assert!(c.is_ok(), "{:?}", c.err());

    let e = Client::connect(&url(port, Some("wrong")))
        .unwrap_err()
        .to_string();
    assert!(e.contains("28P01") || e.contains("password"), "{e}");
}

#[test]
fn cleartext_authentication_works() {
    let port = server(Auth::Cleartext("open"));
    assert!(Client::connect(&url(port, Some("open"))).is_ok());
}

/// When a password is required but not supplied, the error must be clear.
#[test]
fn missing_password_is_explained() {
    let port = server(Auth::Scram("p"));
    let e = Client::connect(&url(port, None)).unwrap_err().to_string();
    assert!(e.contains("PGPASSWORD"), "{e}");
}

/// The connection must stay usable after an error: if `ReadyForQuery` is not
/// consumed, the next query reads the tail of the previous response.
#[test]
fn connection_survives_a_failed_query() {
    let port = server(Auth::Trust);
    let mut c = Client::connect(&url(port, None)).unwrap();
    assert!(c.query("select * from no_such_thing").is_err());
    let r = c.query("select name from t").unwrap();
    assert_eq!(r.rows.len(), 2);
}

// ------------------------------------------------------------------ COPY

/// A server that only produces the COPY stream. `chunks` are sent as
/// `CopyData` frames exactly as given -- they need not align with line
/// boundaries.
fn copy_server(chunks: Vec<Vec<u8>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut len = [0u8; 4];
        s.read_exact(&mut len).unwrap();
        let mut body = vec![0u8; i32::from_be_bytes(len) as usize - 4];
        s.read_exact(&mut body).unwrap();

        let mut out = Vec::new();
        out.extend_from_slice(&framed(b'R', &0i32.to_be_bytes())); // AuthenticationOk
        let mut ps = Vec::new();
        cstr(&mut ps, "server_version");
        cstr(&mut ps, "17.0");
        out.extend_from_slice(&framed(b'S', &ps));
        out.extend_from_slice(&framed(b'Z', b"I"));
        s.write_all(&out).unwrap();

        // Query -> the COPY stream.
        let _ = read_msg(&mut s);
        let mut out = Vec::new();
        // CopyOutResponse: text format, the column count does not matter.
        let mut h = vec![0u8];
        h.extend_from_slice(&0i16.to_be_bytes());
        out.extend_from_slice(&framed(b'H', &h));
        for c in &chunks {
            out.extend_from_slice(&framed(b'd', c));
        }
        out.extend_from_slice(&framed(b'c', b""));
        out.extend_from_slice(&framed(b'C', b"COPY 3\0"));
        out.extend_from_slice(&framed(b'Z', b"I"));
        s.write_all(&out).unwrap();
        // Do not close before Terminate is awaited.
        let mut sink = Vec::new();
        let _ = s.read_to_end(&mut sink);
    });
    port
}

fn lines(port: u16) -> Vec<String> {
    let c = Client::connect(&url(port, None)).unwrap();
    let mut copy = c.copy_out("copy (select 1) to stdout").unwrap();
    let mut out = Vec::new();
    while let Some(l) = copy.next_line().unwrap() {
        out.push(String::from_utf8(l).unwrap());
    }
    out
}

#[test]
fn copy_stream_splits_into_lines() {
    let port = copy_server(vec![b"one\tfive\ntwo\tsix\nthree\tseven\n".to_vec()]);
    assert_eq!(lines(port), vec!["one\tfive", "two\tsix", "three\tseven"]);
}

/// `CopyData` frames do not align with line boundaries: a frame can end
/// mid-line. Without buffering and rejoining, the lines would be split.
#[test]
fn rows_split_across_frames_are_rejoined() {
    let port = copy_server(vec![
        b"one\tfi".to_vec(),
        b"ve\ntwo\ts".to_vec(),
        b"ix\nthree".to_vec(),
        b"\tseven\n".to_vec(),
    ]);
    assert_eq!(lines(port), vec!["one\tfive", "two\tsix", "three\tseven"]);
}

/// Several rows in a single frame.
#[test]
fn many_rows_in_one_frame() {
    let port = copy_server(vec![b"a\nb\nc\nd\n".to_vec()]);
    assert_eq!(lines(port), vec!["a", "b", "c", "d"]);
}

/// Even without a trailing newline the last row must not be lost.
#[test]
fn trailing_row_without_newline_is_kept() {
    let port = copy_server(vec![b"a\nb".to_vec()]);
    assert_eq!(lines(port), vec!["a", "b"]);
}

#[test]
fn empty_copy_stream_yields_nothing() {
    let port = copy_server(vec![]);
    assert!(lines(port).is_empty());
}
