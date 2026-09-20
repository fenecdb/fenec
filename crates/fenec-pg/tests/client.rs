//! Verifies the client side.
//!
//! Two paths are taken. Authentication and queries run against fenecdb's own
//! server -- since both halves of SCRAM are hand-written, that is a good
//! cross-check. Because fenecdb does not speak COPY, that arm is tested
//! against a small fake server that only produces the stream; this also
//! covers the case where `CopyData` frames do not align with line boundaries.
//!
//! The client in `tests/wire.rs` is deliberately independent and keeps
//! verifying the server; the subject of the tests here is the client.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, RwLock};
use fenec_core::prelude::Database;
use fenec_pg::client::{Client, Url};
use fenec_pg::server::Auth;
use fenec_pg::{Config, PgPlugin, Server};

fn start(auth: Auth) -> u16 {
    let mut db = Database::new();
    db.install_plugin(&PgPlugin).unwrap();
    db.execute(&fenec_ql::parse_one("create collection t (name text, score int)").unwrap())
        .unwrap();
    db.execute(&fenec_ql::parse_one(r#"put t [{name: "one", score: 1}, {name: "two", score: 2}]"#).unwrap())
        .unwrap();

    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        auth,
        ..Config::default()
    };
    let server = Server::new(Arc::new(RwLock::new(db)), cfg);
    let listener = server.bind().expect("bind");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    port
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
    let port = start(Auth::Trust);
    let mut c = Client::connect(&url(port, None)).unwrap();
    assert!(!c.server_version.is_empty(), "ParameterStatus was not read");

    let r = c.query("get t select name, score order score asc").unwrap();
    let names: Vec<&str> = r.columns.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, vec!["name", "score"]);
    assert_eq!(r.rows.len(), 2);
    assert_eq!(r.rows[0][0].as_deref(), Some("one"));
    assert_eq!(r.rows[1][1].as_deref(), Some("2"));
}

/// The client half of SCRAM is here, the server half in `scram.rs`. Both
/// being hand-written, they verify each other.
#[test]
fn scram_handshake_succeeds_and_rejects() {
    let port = start(Auth::parse("scram", "right-password").unwrap());
    let c = Client::connect(&url(port, Some("right-password")));
    assert!(c.is_ok(), "{:?}", c.err());

    let e = Client::connect(&url(port, Some("wrong")))
        .unwrap_err()
        .to_string();
    assert!(e.contains("28P01") || e.contains("password"), "{e}");
}

#[test]
fn cleartext_authentication_works() {
    let port = start(Auth::parse("cleartext", "open").unwrap());
    assert!(Client::connect(&url(port, Some("open"))).is_ok());
}

/// When a password is required but not supplied, the error must be clear.
#[test]
fn missing_password_is_explained() {
    let port = start(Auth::parse("scram", "p").unwrap());
    let e = Client::connect(&url(port, None)).unwrap_err().to_string();
    assert!(e.contains("PGPASSWORD"), "{e}");
}

/// The connection must stay usable after an error: if `ReadyForQuery` is not
/// consumed, the next query reads the tail of the previous response.
#[test]
fn connection_survives_a_failed_query() {
    let port = start(Auth::Trust);
    let mut c = Client::connect(&url(port, None)).unwrap();
    assert!(c.query("get no_such_thing").is_err());
    let r = c.query("get t select name").unwrap();
    assert_eq!(r.rows.len(), 2);
}

// ------------------------------------------------------- COPY fake server

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

/// A server that only produces the COPY stream. `chunks` are sent as
/// `CopyData` frames exactly as given -- they need not align with line boundaries.
fn copy_server(chunks: Vec<Vec<u8>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        // StartupMessage: read the length and discard the body.
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
        let mut head = [0u8; 5];
        s.read_exact(&mut head).unwrap();
        let mut q = vec![0u8; i32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize - 4];
        s.read_exact(&mut q).unwrap();

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
