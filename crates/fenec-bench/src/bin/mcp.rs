//! What an MCP tool call costs beside the `/query` it runs: `make
//! mcp-bench`.
//!
//! Over 10 000 rows -- a title, a body under `@text`, a number -- three
//! reads are asked both ways: a row by id (`/query`'s `get ... where id =
//! $1` against the `get` tool and the `query` tool with the same text),
//! and a search by two words (`match ... limit 10` with a snippet against
//! the `search` tool). In process, each through the function that answers
//! it -- `fenec_http::handle` with the request `/query` would be, and
//! `mcp::answer` with the JSON-RPC message -- the median of 20 000 calls;
//! over HTTP, one client keeping its connection to a release fenec-server
//! started with `--mcp`, the p50 and p99 of 3 000 round trips, each MCP
//! request a 2026-07-28 one with the headers its binding mirrors.

#[path = "../http.rs"]
mod http;

use fenec_core::prelude::*;
use fenec_http::access::Who;
use fenec_http::http::{Method, Request};
use fenec_http::mcp;
use std::sync::{Arc, RwLock};
use std::time::Instant;

const ROWS: usize = 10_000;
const IN_PROCESS: usize = 20_000;
const OVER_HTTP: usize = 3_000;
const WORDS: [&str; 8] = ["fox", "dog", "cat", "owl", "bee", "elk", "yak", "eel"];
const TOKEN: &str = "mcp-bench";

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn word(&mut self) -> &'static str {
        WORDS[(self.next() % WORDS.len() as u64) as usize]
    }
}

/// The rows, as the statements that put them, 500 a statement.
fn puts() -> Vec<String> {
    let mut rng = Rng(11);
    let mut out = Vec::new();
    let mut docs = Vec::new();
    for i in 0..ROWS {
        let body: Vec<&str> = (0..12).map(|_| rng.word()).collect();
        docs.push(format!(
            "{{title: \"row {i}\", body: \"{}\", n: {}}}",
            body.join(" "),
            rng.next() % 1000
        ));
        if docs.len() == 500 {
            out.push(format!("put docs [{}]", docs.join(", ")));
            docs.clear();
        }
    }
    out
}

const SCHEMA: &str = "create collection docs (title text, body text @text, n int)";

/// A 2026-07-28 `tools/call` of `tool` with `args`.
fn call(tool: &str, args: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"{tool}","arguments":{args},"_meta":{{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{{}}}}}}}}"#
    )
}

fn query_body(text: &str, params: &str) -> String {
    format!(
        "{{\"query\":{},\"params\":[{params}]}}",
        http::json_string(text)
    )
}

const BY_ID: &str = "get docs where id = $1";
const SEARCH: &str =
    "get docs select id, title, body, n, snippet(body, 30, \"…\", \"**\", \"**\") match body $1 limit 10";

/// The asks, each with the `/query` body and the MCP message that answer
/// the same read, drawn afresh.
fn asks(rng: &mut Rng) -> [(&'static str, String, String); 3] {
    let id = rng.next() % ROWS as u64 + 1;
    let words = format!("{} {}", rng.word(), rng.word());
    [
        (
            "row by id, get tool",
            query_body(BY_ID, &id.to_string()),
            call("get", &format!(r#"{{"collection":"docs","ids":[{id}]}}"#)),
        ),
        (
            "row by id, query tool",
            query_body(BY_ID, &id.to_string()),
            call(
                "query",
                &format!(r#"{{"query":"get docs where id = $1","params":[{id}]}}"#),
            ),
        ),
        (
            "two words, search tool",
            query_body(SEARCH, &http::json_string(&words)),
            call(
                "search",
                &format!(r#"{{"collection":"docs","text":"{words}"}}"#),
            ),
        ),
    ]
}

fn pct(v: &mut [u64], p: f64) -> f64 {
    v.sort_unstable();
    v[((v.len() - 1) as f64 * p) as usize] as f64 / 1000.0
}

fn in_process() {
    let mut db = Database::new();
    for s in std::iter::once(SCHEMA.to_string()).chain(puts()) {
        db.execute(&fenec_ql::parse_one(&s).unwrap()).unwrap();
    }
    let db = Arc::new(RwLock::new(db));
    let opts = mcp::Options {
        calls_per_sec: 0,
        ..mcp::Options::default()
    };
    let cfg = fenec_http::Config {
        mcp: Some(Arc::new(opts.clone())),
        ..fenec_http::Config::default()
    };
    let ctx = mcp::Ctx {
        db: &db,
        cfg: &cfg,
        who: &Who::Full,
        opts: &opts,
        run: mcp::Run::served(),
    };
    println!("in process, the median of {IN_PROCESS} calls (us):");
    println!("  {:<24} {:>9} {:>9} {:>9}", "", "/query", "MCP", "more");
    for k in 0..3 {
        let mut rng = Rng(5);
        let (mut q, mut m) = (Vec::new(), Vec::new());
        for i in 0..IN_PROCESS + 1_000 {
            let ask = asks(&mut rng).into_iter().nth(k).unwrap();
            let req = Request {
                method: Method::Post,
                target: "/query".into(),
                path: "/query".into(),
                query: Vec::new(),
                headers: Vec::new(),
                body: ask.1.into_bytes(),
                keep_alive: true,
            };
            let t = Instant::now();
            let r = fenec_http::handle(&db, &cfg, &req);
            let tq = t.elapsed().as_nanos() as u64;
            assert_eq!(r.status, 200);
            let t = Instant::now();
            let a = mcp::answer(&ctx, &ask.2, None);
            let tm = t.elapsed().as_nanos() as u64;
            fenec_http::statements::clear();
            assert!(a.body.unwrap_or_default().contains("\"isError\":false"));
            // The first thousand warm the parse cache and the text index.
            if i >= 1_000 {
                q.push(tq);
                m.push(tm);
            }
        }
        let (a, b) = (pct(&mut q, 0.5), pct(&mut m, 0.5));
        let name = asks(&mut Rng(5))[k].0;
        println!("  {name:<24} {a:>9.2} {b:>9.2} {:>9.2}", b - a);
    }
}

fn over_http() {
    let dir = std::env::temp_dir().join(format!("fenec-mcp-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("mcp.fenec");
    let _ = std::fs::remove_file(&file);
    let port = http::free_port();
    let _server = http::start_fenec(
        &file,
        port,
        "mcp-bench",
        &["--http-token", TOKEN, "--mcp", "--mcp-calls-per-sec", "0"],
    );
    let addr = format!("127.0.0.1:{port}");
    let mut c = http::Http::connect(&addr);
    let auth = format!("Authorization: Bearer {TOKEN}\r\n");
    for s in std::iter::once(SCHEMA.to_string()).chain(puts()) {
        let (status, out) = c.request_with(
            "POST",
            "/query",
            "application/json",
            &auth,
            query_body(&s, "").as_bytes(),
        );
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(out));
    }
    println!("\nover HTTP, one client, {OVER_HTTP} round trips (us, p50 / p99):");
    println!("  {:<24} {:>17} {:>17}", "", "/query", "MCP");
    for k in 0..3 {
        let mut rng = Rng(9);
        let (mut q, mut m) = (Vec::new(), Vec::new());
        for i in 0..OVER_HTTP + 300 {
            let ask = asks(&mut rng).into_iter().nth(k).unwrap();
            let t = Instant::now();
            let (status, out) = c.request_with(
                "POST",
                "/query",
                "application/json",
                &auth,
                ask.1.as_bytes(),
            );
            assert_eq!(status, 200, "{}", String::from_utf8_lossy(out));
            let tq = t.elapsed().as_nanos() as u64;
            let tool = if k == 2 {
                "search"
            } else if k == 1 {
                "query"
            } else {
                "get"
            };
            let headers = format!(
                "{auth}MCP-Protocol-Version: 2026-07-28\r\nMcp-Method: tools/call\r\nMcp-Name: {tool}\r\n\
                 Accept: application/json, text/event-stream\r\n"
            );
            let t = Instant::now();
            let (status, out) = c.request_with(
                "POST",
                "/_mcp",
                "application/json",
                &headers,
                ask.2.as_bytes(),
            );
            let tm = t.elapsed().as_nanos() as u64;
            assert_eq!(status, 200, "{}", String::from_utf8_lossy(out));
            assert!(String::from_utf8_lossy(out).contains("\"isError\":false"));
            if i >= 300 {
                q.push(tq);
                m.push(tm);
            }
        }
        let name = asks(&mut Rng(9))[k].0;
        println!(
            "  {name:<24} {:>8.1} / {:>6.1} {:>8.1} / {:>6.1}",
            pct(&mut q, 0.5),
            pct(&mut q, 0.99),
            pct(&mut m, 0.5),
            pct(&mut m, 0.99)
        );
    }
    let _ = std::fs::remove_dir_all(dir);
}

fn main() {
    in_process();
    over_http();
}
