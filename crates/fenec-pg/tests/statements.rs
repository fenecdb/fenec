//! What each statement cost, by its shape, on a running `fenec-pg`:
//! `GET /_stats/statements` and `pg_stat_statements` over the pg wire. A
//! statement with its values in the text is counted with the same one run
//! with other values; the rows it returned or changed and its failures go
//! with it; the counts need the server's token, and go on `DELETE`.

use fenec_pg::client::{Client, Url};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const TOKEN: &str = "statements-token";

struct Server {
    child: Child,
    pg: u16,
    http: u16,
    dir: std::path::PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn start() -> Server {
    let dir = std::env::temp_dir().join(format!("fenecpg-statements-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_fenec-pg"))
        .args(["--listen", "127.0.0.1:0", "--http", "127.0.0.1:0"])
        .args(["--http-token", TOKEN])
        .arg("--file")
        .arg(dir.join("s.fenec"))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("could not start fenec-pg");
    let mut err = BufReader::new(child.stderr.take().unwrap());
    let port = |line: &str, after: &str| -> Option<u16> {
        line.split(after)
            .nth(1)?
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .ok()
    };
    let (mut pg, mut http) = (None, None);
    let mut seen = String::new();
    while pg.is_none() || http.is_none() {
        let mut line = String::new();
        if err.read_line(&mut line).unwrap_or(0) == 0 {
            panic!("fenec-pg ended before it listened:\n{seen}");
        }
        seen.push_str(&line);
        if line.contains("postgres://localhost:") {
            pg = port(&line, "localhost:");
        } else if line.contains("listening on: http://127.0.0.1:") {
            http = port(&line, "127.0.0.1:");
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
        dir,
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
        .expect("could not connect")
    }

    fn http(&self, method: &str, path: &str, token: Option<&str>, body: &str) -> (u16, String) {
        let mut s = TcpStream::connect(("127.0.0.1", self.http)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let auth = token.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
        write!(
            s,
            "{method} {path} HTTP/1.1\r\nHost: x\r\n{auth}Content-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
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
}

/// The value after `"key":` in the JSON object for `query`, as written. A
/// shape can hold braces, so the object ends after its last key.
fn field<'a>(json: &'a str, query: &str, key: &str) -> Option<&'a str> {
    let at = json.find(&format!("\"query\":\"{query}\""))?;
    let obj = &json[at..];
    let last = obj.find("\"max_ms\":")?;
    let rest = &obj[..last + obj[last..].find('}')?];
    let v = rest.split(&format!("\"{key}\":")).nth(1)?;
    Some(v.split([',', '}']).next()?.trim_matches('"'))
}

#[test]
fn statements_are_counted_by_their_shape() {
    let s = start();
    let mut c = s.client();
    c.query("create collection t (name text, n int @hash)")
        .unwrap();
    for i in 0..3 {
        c.query(&format!("put t {{name: \"n{i}\", n: {i}}}"))
            .unwrap();
    }
    c.query("get t where n = 1").unwrap();
    c.query("get t where n = 99").unwrap();
    assert!(c.query("get nosuch where n = 1").is_err());
    let (status, _) = s.http("POST", "/query", Some(TOKEN), r#"{"query":"get t count"}"#);
    assert_eq!(status, 200);

    let (status, json) = s.http("GET", "/_stats/statements", Some(TOKEN), "");
    assert_eq!(status, 200, "{json}");
    let put = "put t {name: $1, n: $2}";
    assert_eq!(field(&json, put, "calls"), Some("3"), "{json}");
    assert_eq!(field(&json, put, "rows"), Some("3"), "{json}");
    let get = "get t where n = $1";
    assert_eq!(field(&json, get, "calls"), Some("2"), "{json}");
    assert_eq!(field(&json, get, "rows"), Some("1"), "{json}");
    assert_eq!(
        field(&json, "get nosuch where n = $1", "errors"),
        Some("1"),
        "{json}"
    );
    // Over HTTP a statement is its FenecQL, counted with the pg wire's.
    assert_eq!(field(&json, "get t count", "calls"), Some("1"), "{json}");
    let (status, _) = s.http("GET", "/t?n=eq.1", Some(TOKEN), "");
    assert_eq!(status, 200);
    let (_, json) = s.http("GET", "/_stats/statements", Some(TOKEN), "");
    assert_eq!(field(&json, "GET /t?n=eq.$1", "rows"), Some("1"), "{json}");

    // psql, a monitoring agent: PostgreSQL's view, over the same counts.
    let rows = c
        .query("select query, calls, rows from pg_stat_statements order by calls desc, query")
        .unwrap()
        .rows;
    assert_eq!(
        rows[0],
        vec![Some(put.to_string()), Some("3".into()), Some("3".into())],
        "{rows:?}"
    );

    // The token the metrics take; none is no count.
    assert_eq!(s.http("GET", "/_stats/statements", None, "").0, 401);
    assert_eq!(
        s.http("DELETE", "/_stats/statements", Some(TOKEN), "").0,
        204
    );
    let (_, json) = s.http("GET", "/_stats/statements", Some(TOKEN), "");
    assert!(!json.contains(put), "{json}");
}
