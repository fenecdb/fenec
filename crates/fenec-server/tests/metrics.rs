//! `/_metrics` and `--slow-ms` on a running `fenec-server`: what clients
//! did, counted by whether it wrote; a histogram Prometheus can take apart;
//! the token the scrape needs; and a slow statement in the log with its
//! text, while a fast one stays out of it.

#[path = "support.rs"]
mod support;

use std::collections::HashMap;
use std::time::{Duration, Instant};
use support::{start, tmp, Http, Server};

const TOKEN: &str = "metrics-token";

struct Node {
    server: Server,
    metrics: u16,
}

fn node(name: &str, extra: &[&str]) -> Node {
    let path = tmp("metrics", name);
    let mut args = vec![
        "--metrics",
        "127.0.0.1:0",
        "--http-token",
        TOKEN,
        "--file",
        path.to_str().unwrap(),
    ];
    args.extend(extra);
    let server = start(&args);
    assert!(server.logged("metrics on: http://", Duration::from_secs(10)));
    let log = server.log.lock().unwrap().clone();
    let line = log
        .lines()
        .find(|l| l.contains("metrics on: http://"))
        .unwrap();
    let rest = line.split("127.0.0.1:").nth(1).unwrap();
    let metrics = rest
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap();
    Node { server, metrics }
}

impl Node {
    fn ask(
        &self,
        port: u16,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: &str,
    ) -> (u16, String) {
        let mut c = Http::open(port);
        c.token = token.map(String::from);
        let a = c.ask(method, path, body);
        (a.status, a.body)
    }

    /// Every sample of a scrape, by its name and labels as written.
    fn scrape(&self) -> HashMap<String, f64> {
        let (status, text) = self.ask(self.metrics, "GET", "/_metrics", Some(TOKEN), "");
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
fn reads_and_writes_are_counted_apart() {
    let s = node("counted.fenec", &[]);
    let mut c = s.server.http().with_token(TOKEN);
    c.run("create collection t (name text, n int)");
    for i in 0..3 {
        c.run(&format!("put t {{name: \"n{i}\", n: {i}}}"));
    }
    c.run("get t order name");
    assert!(c.query("get t wher n = 1").is_err());
    assert_eq!(c.ask("POST", "/t", r#"{"name":"h","n":9}"#).status, 201);
    assert_eq!(c.ask("GET", "/t?n=eq.9", "").status, 200);
    assert_eq!(c.ask("GET", "/nosuch", "").status, 404);
    // A read over /query is a read, though it is a POST.
    c.run("get t count");

    let m = s.scrape();
    let n = |k: &str| value(&m, k);
    assert_eq!(n(r#"fenec_statements_total{kind="write"}"#), 5.0);
    // The statement with the typo is counted, as a read: it never got as
    // far as saying what it would do.
    assert_eq!(n(r#"fenec_statements_total{kind="read"}"#), 5.0);
    assert_eq!(n(r#"fenec_statement_errors_total{kind="read"}"#), 2.0);
    assert_eq!(n(r#"fenec_statement_errors_total{kind="write"}"#), 0.0);
    assert_eq!(n(r#"fenec_documents{collection="t"}"#), 4.0);
    assert_eq!(n("fenec_storage_failed"), 0.0);
    assert!(n("fenec_change_sequence") >= 5.0);
    assert!(n("fenec_memory_bytes") > 0.0);
    // The client is still connected; a scrape is nobody's connection.
    assert_eq!(n("fenec_connections"), 1.0);

    // What Prometheus needs of a histogram: buckets that only grow, the last
    // one the count, and a sum.
    for k in ["read", "write"] {
        let labels = format!(r#"kind="{k}""#);
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
    while value(&s.scrape(), "fenec_connections") != 0.0 {
        assert!(
            Instant::now() < deadline,
            "the closed connection is still counted"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn the_scrape_takes_the_servers_tokens() {
    let s = node("tokens.fenec", &["--admin-token", "admin-token"]);
    for port in [s.metrics, s.server.port] {
        assert_eq!(s.ask(port, "GET", "/_metrics", None, "").0, 401);
        assert_eq!(s.ask(port, "GET", "/_metrics", Some("wrong"), "").0, 401);
        for token in [TOKEN, "admin-token"] {
            let (status, body) = s.ask(port, "GET", "/_metrics", Some(token), "");
            assert_eq!(status, 200);
            assert!(
                body.contains("# TYPE fenec_statements_total counter"),
                "{body}"
            );
        }
    }
    // The metrics listener serves nothing else, token or not.
    assert_eq!(s.ask(s.metrics, "GET", "/t", Some(TOKEN), "").0, 404);
}

#[test]
fn a_slow_statement_is_logged_with_its_text() {
    let s = node("slow.fenec", &["--slow-ms", "20"]);
    let mut c = s.server.http().with_token(TOKEN);
    c.run("create collection t (name text, n int)");
    let docs: Vec<String> = (0..20_000)
        .map(|i| format!("{{name: \"row {i}\", n: {i}}}"))
        .collect();
    let t = Instant::now();
    c.run(&format!("put t [{}]", docs.join(", ")));
    assert!(
        t.elapsed() >= Duration::from_millis(20),
        "the put was not slow"
    );
    // A point read by id, far under the threshold, with a mark to look for.
    c.run("get t where id = 987654");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let log = s.server.log.lock().unwrap().clone();
        if let Some(line) = log.lines().find(|l| l.contains(r#""event":"slow""#)) {
            // A JSON line; the request as it came: its line, then its body.
            assert!(
                line.starts_with('{')
                    && line.contains(r#""kind":"write""#)
                    && line.contains(r#""statement":"POST /query "#)
                    && line.contains("put t [{name:"),
                "{line}"
            );
            // Cut short: the statement is a megabyte, the line is not.
            assert!(line.len() < 1400 && line.ends_with(r#"..."}"#), "{line}");
            assert!(!log.contains("987654"), "{log}");
            break;
        }
        assert!(Instant::now() < deadline, "no slow statement in:\n{log}");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(value(&s.scrape(), "fenec_slow_statements_total"), 1.0);
}
