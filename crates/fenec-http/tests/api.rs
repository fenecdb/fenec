//! `fenec-http` end-to-end tests.
//!
//! The server is brought up in-process on `127.0.0.1:0`; the client is raw
//! TCP, so the assertions are on the real byte stream -- not on the
//! leniency of an HTTP library.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use fenec_core::prelude::*;
use fenec_http::{Config, Server};

struct Harness {
    port: u16,
    #[allow(dead_code)]
    db: Arc<RwLock<Database>>,
}

fn seeded() -> Database {
    let mut db = Database::new();
    let run = |db: &mut Database, sql: &str| {
        for stmt in fenec_ql::parse(sql).expect("parse") {
            db.execute(&stmt).expect("execute");
        }
    };
    run(
        &mut db,
        r#"create collection articles (
             title text required,
             tags [text],
             year int @hash,
             summary text,
             published timestamp,
             embed vector<3> @hnsw(cosine, m=8, ef_construction=32)
           )"#,
    );
    run(
        &mut db,
        r#"put articles [
             {title: "rust book", tags: ["rust","book"], year: 2024,
              summary: "a", published: "2024-03-01T00:00:00Z", embed: [1.0, 0.0, 0.0]},
             {title: "wasm guide", tags: ["wasm","rust"], year: 2023,
              published: "2023-05-01T00:00:00Z", embed: [0.9, 0.1, 0.0]},
             {title: "old notebook", tags: [], year: 1999,
              summary: "c", published: "1999-01-01T00:00:00Z", embed: [0.0, 0.0, 1.0]}
           ]"#,
    );
    db
}

fn start(cfg: Config) -> Harness {
    start_with(cfg, seeded())
}

fn start_with(mut cfg: Config, db: Database) -> Harness {
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

struct Res {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Res {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn raw(port: u16, request: &str) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(request.as_bytes()).unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out
}

fn parse_res(text: &str) -> Res {
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text, ""));
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    Res {
        status,
        headers,
        body: body.to_string(),
    }
}

fn call(port: u16, method: &str, target: &str, body: Option<&str>) -> Res {
    call_with(port, method, target, body, &[])
}

fn call_with(
    port: u16,
    method: &str,
    target: &str,
    body: Option<&str>,
    headers: &[(&str, &str)],
) -> Res {
    let mut req = format!("{method} {target} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    match body {
        Some(b) => {
            req.push_str(&format!("Content-Length: {}\r\n\r\n{b}", b.len()));
        }
        None => req.push_str("\r\n"),
    }
    parse_res(&raw(port, &req))
}

fn get(port: u16, target: &str) -> Res {
    call(port, "GET", target, None)
}

/// Number of rows in the response body (a JSON array).
fn rows(body: &str) -> usize {
    let t = body.trim();
    assert!(t.starts_with('['), "expected an array: {t}");
    if t == "[]" {
        0
    } else {
        t.matches("},{").count() + 1
    }
}

// ---------------------------------------------------------------- reading

#[test]
fn service_info_and_schemas() {
    let h = start(Config::default());

    let r = get(h.port, "/");
    assert_eq!(r.status, 200);
    assert!(r.body.contains("\"fenecdb\""), "{}", r.body);
    assert!(r.body.contains("\"articles\""), "{}", r.body);

    let r = get(h.port, "/collections");
    assert_eq!(r.status, 200);
    assert!(r.body.contains("\"name\":\"articles\""), "{}", r.body);
    assert!(r.body.contains("\"type\":\"vector<3>\""), "{}", r.body);
    assert!(r.body.contains("\"index\":\"hash\""), "{}", r.body);
    assert!(r.body.contains("\"required\":true"), "{}", r.body);
}

#[test]
fn select_filter_order_limit() {
    let h = start(Config::default());

    assert_eq!(rows(&get(h.port, "/articles").body), 3);

    let r = get(h.port, "/articles?select=title&year=gte.2024");
    assert_eq!(r.status, 200);
    assert_eq!(r.body.trim(), "[{\"title\":\"rust book\"}]");

    // A value with no operator prefix is an equality.
    assert_eq!(rows(&get(h.port, "/articles?year=2023").body), 1);
    assert_eq!(rows(&get(h.port, "/articles?year=eq.2023").body), 1);

    // Two conditions on the same field are `and`ed.
    assert_eq!(
        rows(&get(h.port, "/articles?year=gte.2000&year=lt.2024").body),
        1
    );

    // List, text, null.
    assert_eq!(rows(&get(h.port, "/articles?tags=has.rust").body), 2);
    assert_eq!(rows(&get(h.port, "/articles?title=like.GUIDE").body), 1);
    assert_eq!(rows(&get(h.port, "/articles?summary=is.null").body), 1);
    assert_eq!(rows(&get(h.port, "/articles?summary=not.is.null").body), 2);
    assert_eq!(rows(&get(h.port, "/articles?year=in.(1999,2023)").body), 2);
    assert_eq!(rows(&get(h.port, "/articles?year=not.eq.1999").body), 2);

    // Timestamp: ISO and epoch ms.
    assert_eq!(
        rows(&get(h.port, "/articles?published=gte.2024-01-01").body),
        1
    );
    assert_eq!(
        rows(&get(h.port, "/articles?published=gte.1704067200000").body),
        1
    );

    // Multi-key ordering and pagination.
    let r = get(h.port, "/articles?select=year&order=year.desc");
    assert_eq!(r.body.trim(), "[{\"year\":2024},{\"year\":2023},{\"year\":1999}]");
    let r = get(h.port, "/articles?select=year&order=year.asc&limit=2&offset=1");
    assert_eq!(r.body.trim(), "[{\"year\":2023},{\"year\":2024}]");

    // id is not a schema field but can be filtered and ordered.
    assert_eq!(rows(&get(h.port, "/articles?id=gte.0&order=id.desc").body), 3);
}

#[test]
fn count_and_free_where() {
    let h = start(Config::default());

    let r = get(h.port, "/articles?count");
    assert_eq!(r.body.trim(), "{\"count\":3}");
    let r = get(h.port, "/articles?year=gte.2023&count=true");
    assert_eq!(r.body.trim(), "{\"count\":2}");

    // `where` is a free FenecQL expression -- `or` and function calls pass too.
    let r = get(h.port, "/articles?where=year%20%3D%201999%20or%20year%20%3D%202024");
    assert_eq!(rows(&r.body), 2);
    let r = get(h.port, "/articles?where=lower(title)%20~%20%22rust%22");
    assert_eq!(rows(&r.body), 1);
    // It accepts no clause beyond the condition.
    let r = get(h.port, "/articles?where=year%20%3E%200%20limit%201");
    assert_eq!(r.status, 400);

    // `count` does not combine with the other clauses.
    assert_eq!(get(h.port, "/articles?count&limit=2").status, 400);
}

#[test]
fn near_is_a_post_endpoint() {
    let h = start(Config::default());

    let r = call(
        h.port,
        "POST",
        "/articles/near",
        Some(r#"{"vector":[1.0,0.0,0.0],"limit":2,"select":["title"]}"#),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.body.contains("rust book"), "{}", r.body);
    assert!(r.body.contains("_score"), "{}", r.body);
    assert_eq!(rows(&r.body), 2);

    // The filter can come from the body (`where`) and the query string together.
    let r = call(
        h.port,
        "POST",
        "/articles/near?year=lt.2024",
        Some(r#"{"vector":[1.0,0.0,0.0],"limit":5,"where":"tags has \"rust\""}"#),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(rows(&r.body), 1);
    assert!(r.body.contains("wasm guide"), "{}", r.body);

    // `field` is optional with a single vector field, and a wrong type is rejected.
    let r = call(
        h.port,
        "POST",
        "/articles/near",
        Some(r#"{"field":"title","vector":[1.0,0.0,0.0]}"#),
    );
    assert_eq!(r.status, 400, "{}", r.body);

    // A vector cannot be filtered in the query string -- the message shows the way.
    let r = get(h.port, "/articles?embed=eq.1");
    assert_eq!(r.status, 400);
    assert!(r.body.contains("near"), "{}", r.body);
}

// ---------------------------------------------------------------- writing

#[test]
fn insert_update_delete() {
    let h = start(Config::default());

    let r = call(
        h.port,
        "POST",
        "/articles",
        Some(r#"{"title":"new","year":2025,"embed":[0.0,1.0,0.0]}"#),
    );
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body.trim(), "{\"inserted\":1}");

    let r = call(
        h.port,
        "POST",
        "/articles",
        Some(r#"[{"title":"a","year":2026},{"title":"b","year":2026}]"#),
    );
    assert_eq!(r.body.trim(), "{\"inserted\":2}");
    assert_eq!(get(h.port, "/articles?count").body.trim(), "{\"count\":6}");

    let r = call(
        h.port,
        "PATCH",
        "/articles?year=eq.2026",
        Some(r#"{"summary":"bulk"}"#),
    );
    assert_eq!(r.body.trim(), "{\"updated\":2}");
    assert_eq!(
        get(h.port, "/articles?summary=eq.bulk&count").body.trim(),
        "{\"count\":2}"
    );

    let r = call(h.port, "DELETE", "/articles?year=gte.2025", None);
    assert_eq!(r.body.trim(), "{\"deleted\":3}");
    assert_eq!(get(h.port, "/articles?count").body.trim(), "{\"count\":3}");
}

/// An unfiltered write accidentally covers the whole collection; we require
/// an explicit path.
#[test]
fn unfiltered_writes_need_an_explicit_path() {
    let h = start(Config::default());

    let r = call(h.port, "DELETE", "/articles", None);
    assert_eq!(r.status, 400);
    assert!(r.body.contains("/articles/all"), "{}", r.body);

    let r = call(h.port, "PATCH", "/articles", Some(r#"{"summary":"x"}"#));
    assert_eq!(r.status, 400);

    // `/all` takes no filter: writing both blurs the intent.
    let r = call(h.port, "DELETE", "/articles/all?year=eq.1999", None);
    assert_eq!(r.status, 400);

    let r = call(h.port, "PATCH", "/articles/all", Some(r#"{"summary":"all"}"#));
    assert_eq!(r.body.trim(), "{\"updated\":3}");
    let r = call(h.port, "DELETE", "/articles/all", None);
    assert_eq!(r.body.trim(), "{\"deleted\":3}");
    assert_eq!(get(h.port, "/articles?count").body.trim(), "{\"count\":0}");
}

#[test]
fn errors_carry_useful_status_codes() {
    let h = start(Config::default());

    assert_eq!(get(h.port, "/nosuch").status, 404);
    assert_eq!(get(h.port, "/articles?nofield=eq.1").status, 404);
    assert_eq!(get(h.port, "/articles?select=nofield").status, 404);
    assert_eq!(get(h.port, "/articles?year=gte.abc").status, 400);
    assert_eq!(get(h.port, "/articles?limit=lots").status, 400);
    assert_eq!(get(h.port, "/articles?tags=eq.rust").status, 400);
    assert_eq!(get(h.port, "/articles/near").status, 404);
    assert_eq!(call(h.port, "POST", "/articles", Some("{broken")).status, 400);
    assert_eq!(
        call(h.port, "POST", "/articles", Some(r#"{"nofield":1}"#)).status,
        400
    );
    assert_eq!(call(h.port, "PUT", "/articles/near", None).status, 404);

    // The error body is always JSON.
    let r = get(h.port, "/nosuch");
    assert!(r.body.starts_with("{\"error\":"), "{}", r.body);
}

// --------------------------------------------------------------- security

#[test]
fn bearer_token_is_required_when_set() {
    let h = start(Config {
        token: Some("secret".into()),
        ..Config::default()
    });

    let r = get(h.port, "/articles");
    assert_eq!(r.status, 401);
    assert_eq!(r.header("WWW-Authenticate"), Some("Bearer"));

    let r = call_with(h.port, "GET", "/articles", None, &[("Authorization", "Bearer wrong")]);
    assert_eq!(r.status, 401);

    let r = call_with(h.port, "GET", "/articles", None, &[("Authorization", "Bearer secret")]);
    assert_eq!(r.status, 200);
}

#[test]
fn read_only_refuses_writes_before_routing() {
    let h = start(Config {
        read_only: true,
        ..Config::default()
    });

    assert_eq!(get(h.port, "/articles").status, 200);
    // `near`, which is a read, works too.
    let r = call(h.port, "POST", "/articles/near", Some(r#"{"vector":[1.0,0.0,0.0]}"#));
    assert_eq!(r.status, 200, "{}", r.body);

    for (m, t, b) in [
        ("POST", "/articles", Some(r#"{"title":"x"}"#)),
        ("PATCH", "/articles/all", Some(r#"{"summary":"x"}"#)),
        ("DELETE", "/articles/all", None),
        // 403 even when the collection does not exist: no path info leaks.
        ("DELETE", "/nosuch/all", None),
    ] {
        assert_eq!(call(h.port, m, t, b).status, 403, "{m} {t}");
    }
}

#[test]
fn remote_bind_without_token_is_refused() {
    let server = Server::new(
        Arc::new(RwLock::new(Database::new())),
        Config {
            addr: "0.0.0.0:0".into(),
            ..Config::default()
        },
    );
    let e = server.bind().unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(e.to_string().contains("--http-token"), "{e}");

    // With a token it is allowed.
    let ok = Server::new(
        Arc::new(RwLock::new(Database::new())),
        Config {
            addr: "127.0.0.1:0".into(),
            token: Some("t".into()),
            ..Config::default()
        },
    );
    assert!(ok.bind().is_ok());
}

// --------------------------------------------------------------- protocol

#[test]
fn cors_headers_only_when_configured() {
    let h = start(Config::default());
    assert!(get(h.port, "/articles")
        .header("Access-Control-Allow-Origin")
        .is_none());

    let h = start(Config {
        cors: Some("*".into()),
        ..Config::default()
    });
    let r = get(h.port, "/articles");
    assert_eq!(r.header("Access-Control-Allow-Origin"), Some("*"));

    // Preflight never reaches the database: 204 even on an unknown path.
    let r = call(h.port, "OPTIONS", "/nosuch", None);
    assert_eq!(r.status, 204);
    assert!(r
        .header("Access-Control-Allow-Methods")
        .is_some_and(|v| v.contains("PATCH")));
}

#[test]
fn keep_alive_serves_two_requests_on_one_connection() {
    let h = start(Config::default());
    let mut s = TcpStream::connect(("127.0.0.1", h.port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(
        b"GET /articles?count HTTP/1.1\r\nHost: t\r\n\r\n\
          GET /articles?year=eq.1999&count HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
    )
    .unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    assert_eq!(out.matches("HTTP/1.1 200").count(), 2, "{out}");
    assert!(out.contains("{\"count\":3}"), "{out}");
    assert!(out.contains("{\"count\":1}"), "{out}");
}

#[test]
fn head_sends_headers_without_body() {
    let h = start(Config::default());
    let r = call(h.port, "HEAD", "/articles?count", None);
    assert_eq!(r.status, 200);
    assert_eq!(r.header("Content-Length"), Some("11"));
    assert_eq!(r.body, "");
}

#[test]
fn oversized_and_chunked_bodies_are_refused() {
    let h = start(Config {
        max_body: 32,
        ..Config::default()
    });

    let big = "x".repeat(64);
    let r = call(h.port, "POST", "/articles", Some(&big));
    assert_eq!(r.status, 413);

    let r = parse_res(&raw(
        h.port,
        "POST /articles HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
    ));
    assert_eq!(r.status, 411);
}

#[test]
fn percent_encoded_values_survive() {
    let mut db = seeded();
    for stmt in fenec_ql::parse(r#"put articles {title: "inner space & symbol", year: 2020}"#).unwrap() {
        db.execute(&stmt).unwrap();
    }
    let h = start_with(Config::default(), db);

    let r = get(h.port, "/articles?title=eq.inner%20space%20%26%20symbol&select=year");
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body.trim(), "[{\"year\":2020}]");

    // `+` is a space too.
    let r = get(h.port, "/articles?title=like.inner+space&count");
    assert_eq!(r.body.trim(), "{\"count\":1}");
}

// ---------------------------------------------------------------- raw query

/// `POST /query` opens everything the REST surface does not cover: DDL, `or`
/// groups, function calls. It exists so the query builder in the browser can
/// hand the same text here too.
#[test]
fn raw_fenecql_endpoint() {
    let h = start(Config::default());
    let q = |body: &str| call(h.port, "POST", "/query", Some(body));

    let r = q(r#"{"query":"get articles select title where year >= $1","params":[2024]}"#);
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body.trim(), "[{\"title\":\"rust book\"}]");

    // A vector parameter arrives as a nested array.
    let r = q(r#"{"query":"get articles select title near embed $1 limit 1","params":[[1.0,0.0,0.0]]}"#);
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.body.contains("rust book"), "{}", r.body);

    // A write: the number of affected rows.
    let r = q(r#"{"query":"put articles {title: $1, year: 2030}","params":["raw"]}"#);
    assert_eq!(r.body.trim(), "{\"affected\":1}");
    assert_eq!(get(h.port, "/articles?count").body.trim(), "{\"count\":4}");

    // DDL: it has no equivalent on the REST surface.
    let r = q(r#"{"query":"create collection notes (text text)"}"#);
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.body.contains("notes"), "{}", r.body);
    assert!(get(h.port, "/").body.contains("notes"));

    // `collections` returns the schema list.
    let r = q(r#"{"query":"collections"}"#);
    assert!(r.body.contains("\"fields\""), "{}", r.body);

    // Errors are still JSON + the right status.
    assert_eq!(q(r#"{"query":"get nosuch"}"#).status, 404);
    assert_eq!(q(r#"{"query":"this is not a query"}"#).status, 400);
    assert_eq!(q(r#"{"params":[1]}"#).status, 400);
    assert_eq!(q("[]").status, 400);
    // A single statement: two in a row are rejected.
    assert_eq!(q(r#"{"query":"get articles get articles"}"#).status, 400);
}

#[test]
fn raw_query_respects_read_only() {
    let h = start(Config {
        read_only: true,
        ..Config::default()
    });
    let q = |body: &str| call(h.port, "POST", "/query", Some(body));

    assert_eq!(q(r#"{"query":"get articles"}"#).status, 200);
    assert_eq!(q(r#"{"query":"del articles"}"#).status, 403);
    assert_eq!(q(r#"{"query":"drop collection articles"}"#).status, 403);
}

#[test]
fn raw_query_needs_the_token_too() {
    let h = start(Config {
        token: Some("secret".into()),
        ..Config::default()
    });
    assert_eq!(call(h.port, "POST", "/query", Some(r#"{"query":"get articles"}"#)).status, 401);
    let r = call_with(
        h.port,
        "POST",
        "/query",
        Some(r#"{"query":"get articles"}"#),
        &[("Authorization", "Bearer secret")],
    );
    assert_eq!(r.status, 200);
}
