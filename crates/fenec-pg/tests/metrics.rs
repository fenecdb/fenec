//! `/_metrics` and `--slow-ms` on a running `fenec-pg`: what a pg and an
//! HTTP client did, counted apart; a histogram Prometheus can take apart;
//! the token the scrape needs; and a slow statement in the log with its
//! text, while a fast one stays out of it.

use fenec_pg::client::{Client, Url};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TOKEN: &str = "metrics-token";

struct Server {
    child: Child,
    pg: u16,
    http: u16,
    metrics: u16,
    log: Arc<Mutex<String>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start(name: &str, extra: &[&str]) -> Server {
    let dir = std::env::temp_dir().join(format!("fenecpg-metrics-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path: PathBuf = dir.join(name);
    let _ = std::fs::remove_file(&path);
    let mut child = Command::new(env!("CARGO_BIN_EXE_fenec-pg"))
        .args(["--listen", "127.0.0.1:0", "--http", "127.0.0.1:0"])
        .args(["--metrics", "127.0.0.1:0", "--http-token", TOKEN])
        .arg("--file")
        .arg(&path)
        .args(extra)
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
    let (mut pg, mut http, mut metrics) = (None, None, None);
    let mut seen = String::new();
    while pg.is_none() || http.is_none() || metrics.is_none() {
        let mut line = String::new();
        if err.read_line(&mut line).unwrap_or(0) == 0 {
            panic!("fenec-pg ended before it listened:\n{seen}");
        }
        seen.push_str(&line);
        if line.contains("postgres://localhost:") {
            pg = port(&line, "localhost:");
        } else if line.contains("metrics on: http://127.0.0.1:") {
            metrics = port(&line, "127.0.0.1:");
        } else if line.contains("listening on: http://127.0.0.1:") {
            http = port(&line, "127.0.0.1:");
        }
    }
    // The rest of stderr is kept: the slow-statement log is read from it.
    let log = Arc::new(Mutex::new(seen));
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
        pg: pg.unwrap(),
        http: http.unwrap(),
        metrics: metrics.unwrap(),
        log,
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

    /// Status and body of one request.
    fn http(
        &self,
        port: u16,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: &str,
    ) -> (u16, String) {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
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

    /// Every sample of a scrape, by its name and labels as written.
    fn scrape(&self) -> HashMap<String, f64> {
        let (status, text) = self.http(self.metrics, "GET", "/_metrics", Some(TOKEN), "");
        assert_eq!(status, 200, "{text}");
        text.lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
            .map(|l| {
                let (k, v) = l.rsplit_once(' ').expect(l);
                (k.to_string(), v.parse().expect(l))
            })
            .collect()
    }
}

fn value(m: &HashMap<String, f64>, key: &str) -> f64 {
    *m.get(key)
        .unwrap_or_else(|| panic!("no `{key}` in the scrape"))
}

#[test]
fn each_transport_is_counted_apart() {
    let s = start("counted.fenec", &[]);
    let mut c = s.client();
    c.query("create collection t (name text, n int)").unwrap();
    for i in 0..3 {
        c.query(&format!("put t {{name: \"n{i}\", n: {i}}}"))
            .unwrap();
    }
    c.query("get t order name").unwrap();
    assert!(c.query("get t wher n = 1").is_err());

    let (status, _) = s.http(s.http, "POST", "/t", Some(TOKEN), r#"{"name":"h","n":9}"#);
    assert_eq!(status, 201);
    let (status, _) = s.http(s.http, "GET", "/t?n=eq.9", Some(TOKEN), "");
    assert_eq!(status, 200);
    let (status, _) = s.http(s.http, "GET", "/nosuch", Some(TOKEN), "");
    assert_eq!(status, 404);
    // A read over /query is a read, though it is a POST.
    let (status, _) = s.http(
        s.http,
        "POST",
        "/query",
        Some(TOKEN),
        r#"{"query":"get t count"}"#,
    );
    assert_eq!(status, 200);

    let m = s.scrape();
    let n = |k: &str| value(&m, k);
    assert_eq!(
        n(r#"fenec_statements_total{transport="pg",kind="write"}"#),
        4.0
    );
    // The statement with the typo is counted, as a read: it never got as
    // far as saying what it would do.
    assert_eq!(
        n(r#"fenec_statements_total{transport="pg",kind="read"}"#),
        2.0
    );
    assert_eq!(
        n(r#"fenec_statement_errors_total{transport="pg",kind="read"}"#),
        1.0
    );
    assert_eq!(
        n(r#"fenec_statement_errors_total{transport="pg",kind="write"}"#),
        0.0
    );
    assert_eq!(
        n(r#"fenec_statements_total{transport="http",kind="write"}"#),
        1.0
    );
    assert_eq!(
        n(r#"fenec_statements_total{transport="http",kind="read"}"#),
        3.0
    );
    assert_eq!(
        n(r#"fenec_statement_errors_total{transport="http",kind="read"}"#),
        1.0
    );
    assert_eq!(n(r#"fenec_documents{collection="t"}"#), 4.0);
    assert_eq!(n("fenec_storage_failed"), 0.0);
    assert!(n("fenec_change_sequence") >= 5.0);
    assert!(n("fenec_memory_bytes") > 0.0);
    // The client is still connected; a scrape is nobody's connection.
    assert_eq!(n(r#"fenec_connections{transport="pg"}"#), 1.0);
    assert_eq!(n(r#"fenec_connections{transport="http"}"#), 0.0);

    // What Prometheus needs of a histogram: buckets that only grow, the last
    // one the count, and a sum.
    for (t, k) in [
        ("pg", "read"),
        ("pg", "write"),
        ("http", "read"),
        ("http", "write"),
    ] {
        let labels = format!(r#"transport="{t}",kind="{k}""#);
        let mut buckets: Vec<(f64, f64)> = m
            .iter()
            .filter_map(|(key, v)| {
                let rest = key.strip_prefix(&format!(
                    "fenec_statement_duration_seconds_bucket{{{labels},le=\""
                ))?;
                let le = rest.strip_suffix("\"}")?;
                Some((
                    if le == "+Inf" {
                        f64::INFINITY
                    } else {
                        le.parse().unwrap()
                    },
                    *v,
                ))
            })
            .collect();
        buckets.sort_by(|a, b| a.0.total_cmp(&b.0));
        assert_eq!(buckets.len(), 17, "{labels}");
        assert!(
            buckets.windows(2).all(|w| w[0].1 <= w[1].1),
            "{labels}: {buckets:?}"
        );
        let count = n(&format!(
            "fenec_statement_duration_seconds_count{{{labels}}}"
        ));
        assert_eq!(buckets.last().unwrap().1, count, "{labels}");
        assert_eq!(n(&format!("fenec_statements_total{{{labels}}}")), count);
        let sum = n(&format!("fenec_statement_duration_seconds_sum{{{labels}}}"));
        assert_eq!(sum > 0.0, count > 0.0, "{labels}");
    }

    drop(c);
    let deadline = Instant::now() + Duration::from_secs(10);
    while value(&s.scrape(), r#"fenec_connections{transport="pg"}"#) != 0.0 {
        assert!(
            Instant::now() < deadline,
            "the closed session is still counted"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn the_scrape_takes_the_servers_tokens() {
    let s = start("tokens.fenec", &["--admin-token", "admin-token"]);
    for port in [s.metrics, s.http] {
        let (status, _) = s.http(port, "GET", "/_metrics", None, "");
        assert_eq!(status, 401);
        let (status, _) = s.http(port, "GET", "/_metrics", Some("wrong"), "");
        assert_eq!(status, 401);
        for token in [TOKEN, "admin-token"] {
            let (status, body) = s.http(port, "GET", "/_metrics", Some(token), "");
            assert_eq!(status, 200);
            assert!(
                body.contains("# TYPE fenec_statements_total counter"),
                "{body}"
            );
        }
    }
    // The metrics listener serves nothing else, token or not.
    let (status, _) = s.http(s.metrics, "GET", "/t", Some(TOKEN), "");
    assert_eq!(status, 404);
}

#[test]
fn a_slow_statement_is_logged_with_its_text() {
    let s = start("slow.fenec", &["--slow-ms", "20"]);
    let mut c = s.client();
    c.query("create collection t (name text, n int)").unwrap();
    let docs: Vec<String> = (0..20_000)
        .map(|i| format!("{{name: \"row {i}\", n: {i}}}"))
        .collect();
    let t = Instant::now();
    c.query(&format!("put t [{}]", docs.join(", "))).unwrap();
    assert!(
        t.elapsed() >= Duration::from_millis(20),
        "the put was not slow"
    );
    // A point read by id, far under the threshold, with a mark to look for.
    c.query("get t where id = 987654").unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let log = s.log.lock().unwrap().clone();
        if let Some(line) = log.lines().find(|l| l.starts_with("slow statement: ")) {
            assert!(
                line.contains(" ms, pg write: put t [{name: \"row 0\""),
                "{line}"
            );
            // Cut short: the statement is a megabyte, the line is not.
            assert!(line.len() < 1200 && line.ends_with("..."), "{line}");
            assert!(!log.contains("987654"), "{log}");
            break;
        }
        assert!(Instant::now() < deadline, "no slow statement in:\n{log}");
        std::thread::sleep(Duration::from_millis(20));
    }
    let m = s.scrape();
    assert_eq!(
        value(&m, r#"fenec_slow_statements_total{transport="pg"}"#),
        1.0
    );
    assert_eq!(
        value(&m, r#"fenec_slow_statements_total{transport="http"}"#),
        0.0
    );
}
