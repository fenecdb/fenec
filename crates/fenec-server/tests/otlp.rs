//! An OTLP/HTTP receiver of a page, in the test's process: it takes `POST
//! /v1/traces` as a collector does, holds every span it was sent, and
//! answers 200 -- or, told to, takes the connection and never answers. The
//! router's tests include it too (`#[path]`): the spans of a router and
//! its node, one tree.

#![allow(dead_code)]

use fenec_core::prelude::*;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A span as it arrived, every part a collector reads.
#[derive(Clone, Debug)]
pub struct Got {
    pub service: String,
    pub trace: String,
    pub id: String,
    pub parent: Option<String>,
    pub state: Option<String>,
    pub name: String,
    pub kind: i64,
    pub start: u64,
    pub end: u64,
    pub attrs: Vec<(String, String)>,
    pub error: bool,
}

impl Got {
    pub fn attr(&self, key: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// What a post carried besides its spans.
#[derive(Clone, Debug)]
pub struct Post {
    pub path: String,
    pub headers: Vec<(String, String)>,
}

#[derive(Default)]
struct Held {
    spans: Vec<Got>,
    posts: Vec<Post>,
}

pub struct Receiver {
    pub addr: String,
    held: Arc<Mutex<Held>>,
}

impl Receiver {
    /// Listens on a port of its own and answers every post 200.
    pub fn start() -> Receiver {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let held = Arc::new(Mutex::new(Held::default()));
        let into = Arc::clone(&held);
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                let into = Arc::clone(&into);
                std::thread::spawn(move || serve(s, &into));
            }
        });
        Receiver { addr, held }
    }

    /// `http://` and the address: what `--otlp-endpoint` takes.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn spans(&self) -> Vec<Got> {
        self.held.lock().unwrap().spans.clone()
    }

    pub fn posts(&self) -> Vec<Post> {
        self.held.lock().unwrap().posts.clone()
    }

    /// The spans once `done` holds of them -- the event a test waits for
    /// -- or a panic naming `what` after 30 s.
    pub fn until(&self, what: &str, done: impl Fn(&[Got]) -> bool) -> Vec<Got> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let spans = self.spans();
            if done(&spans) {
                return spans;
            }
            if Instant::now() > deadline {
                panic!("{what} never arrived; the receiver holds {spans:#?}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// A port that takes a connection and never answers: the collector that
/// hangs, whose posts run out their timeout.
pub fn silent() -> (TcpListener, String) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    (l, url)
}

/// A port nothing listens on: the collector that is down.
pub fn closed() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    drop(l);
    url
}

fn serve(s: TcpStream, held: &Mutex<Held>) {
    let mut r = BufReader::new(s.try_clone().unwrap());
    let mut w = s;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
        let mut headers = Vec::new();
        let mut len = 0;
        loop {
            let mut h = String::new();
            if r.read_line(&mut h).unwrap_or(0) == 0 {
                return;
            }
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                if k.eq_ignore_ascii_case("content-length") {
                    len = v.trim().parse().unwrap();
                }
                headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
            }
        }
        let mut body = vec![0; len];
        r.read_exact(&mut body).unwrap();
        let text = String::from_utf8(body).expect("a post is UTF-8");
        let spans = parse(&text);
        {
            let mut held = held.lock().unwrap();
            held.posts.push(Post { path, headers });
            held.spans.extend(spans);
        }
        let _ = w.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
        );
    }
}

/// `ExportTraceServiceRequest`'s JSON, held to OTLP's mapping as it is
/// read: a field missing or of the wrong kind is a panic.
fn parse(text: &str) -> Vec<Got> {
    let v = fenec_core::json::parse_json(text).unwrap_or_else(|e| panic!("{e}: {text}"));
    let list = |v: &Value, k: &str| -> Vec<Value> {
        match v.member(k) {
            Some(Value::List(l)) => l.clone(),
            other => panic!("{k} is not a list: {other:?} in {text}"),
        }
    };
    let text_of = |v: &Value, k: &str| -> Option<String> {
        match v.member(k) {
            Some(Value::Text(t)) => Some(t.clone()),
            None => None,
            other => panic!("{k} is not a string: {other:?}"),
        }
    };
    let attrs = |v: &Value| -> Vec<(String, String)> {
        list(v, "attributes")
            .iter()
            .map(|a| {
                let key = text_of(a, "key").expect("an attribute's key");
                let value = a.member("value").expect("an attribute's value");
                let s = match (
                    value.member("stringValue"),
                    value.member("intValue"),
                    value.member("boolValue"),
                ) {
                    (Some(Value::Text(t)), None, None) => t.clone(),
                    // OTLP's JSON writes a 64-bit int as a string.
                    (None, Some(Value::Text(t)), None) => {
                        t.parse::<i64>().expect("an intValue is a number");
                        t.clone()
                    }
                    (None, None, Some(Value::Bool(b))) => b.to_string(),
                    other => panic!("{key}: {other:?}"),
                };
                (key, s)
            })
            .collect()
    };
    let hex = |s: &str, n: usize| {
        assert!(
            s.len() == n
                && s.bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "{s} is not {n} hex digits"
        );
        assert!(s.bytes().any(|b| b != b'0'), "an id of zeros");
    };
    let mut out = Vec::new();
    for rs in list(&v, "resourceSpans") {
        let resource = rs.member("resource").expect("a resource");
        let service = attrs(resource)
            .into_iter()
            .find(|(k, _)| k == "service.name")
            .map(|(_, v)| v)
            .expect("service.name");
        for ss in list(&rs, "scopeSpans") {
            let scope = ss.member("scope").expect("a scope");
            assert_eq!(text_of(scope, "name").as_deref(), Some("fenecdb"));
            for s in list(&ss, "spans") {
                let trace = text_of(&s, "traceId").unwrap();
                let id = text_of(&s, "spanId").unwrap();
                hex(&trace, 32);
                hex(&id, 16);
                let parent = text_of(&s, "parentSpanId");
                if let Some(p) = &parent {
                    hex(p, 16);
                }
                let nanos = |k: &str| -> u64 { text_of(&s, k).unwrap().parse().unwrap() };
                let (start, end) = (nanos("startTimeUnixNano"), nanos("endTimeUnixNano"));
                assert!(start > 1_600_000_000_000_000_000 && end >= start);
                let kind = match s.member("kind") {
                    Some(Value::Int(k)) if (1..=5).contains(k) => *k,
                    other => panic!("kind {other:?}"),
                };
                let error = match s.member("status").and_then(|st| st.member("code")) {
                    None => false,
                    Some(Value::Int(2)) => true,
                    other => panic!("status {other:?}"),
                };
                out.push(Got {
                    service: service.clone(),
                    attrs: attrs(&s),
                    state: text_of(&s, "traceState"),
                    name: text_of(&s, "name").unwrap(),
                    trace,
                    id,
                    parent,
                    kind,
                    start,
                    end,
                    error,
                });
            }
        }
    }
    out
}

/// The span of `spans` whose id is `id`.
pub fn by_id<'a>(spans: &'a [Got], id: &str) -> &'a Got {
    spans
        .iter()
        .find(|s| s.id == id)
        .unwrap_or_else(|| panic!("no span {id} in {spans:#?}"))
}

/// The children of `parent`, by name.
pub fn children<'a>(spans: &'a [Got], parent: &Got, name: &str) -> Vec<&'a Got> {
    spans
        .iter()
        .filter(|s| s.parent.as_deref() == Some(parent.id.as_str()) && s.name == name)
        .collect()
}
