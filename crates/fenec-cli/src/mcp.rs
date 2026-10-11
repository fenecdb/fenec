//! `fenec mcp`: the MCP server over stdio, for a client that starts its
//! tools as processes -- Claude Code's and Cursor's `command`, Claude
//! Desktop's configuration.
//!
//! Over a file (`--file`) the session holds the database itself, as the
//! native library does: it takes `<file>.lock`, so a server or an app over
//! the same file refuses it and it refuses them, and each write is fsynced
//! before it is answered. `--read-only` takes no lock and reads the file
//! as it stood when the session began, beside a server writing it. Over a
//! server (`--url`) it posts each message to the server's endpoint, where
//! the tools run under the server's options and the token's rules: the
//! token comes from the environment, `FENEC_TOKEN`, never from argv, which
//! `ps` shows.

use fenec_http::mcp;
use std::io;
use std::sync::{Arc, RwLock};

const USAGE: &str = "\
usage: fenec mcp --file <path> [options]
       fenec mcp --url <http://host:port/_mcp>

MCP, the Model Context Protocol, over stdio: a message a line on stdin, an
answer a line on stdout, logs on stderr.

  --file <path>           the database: made when missing, locked while the
                          session lasts (<path>.lock), each write fsynced
    --read-only           take no lock and write nothing: the file as it
                          stood when the session began, beside a server
    --write               the write tool: put, insert, set and del
    --memory <collection> remember, recall and forget, in this collection
    --max-rows <n>        the most rows a tool answers; past it the call is
                          refused, never cut  default: 100
    --max-bytes <size>    the most bytes a tool answers  default: 64K
  --url <url>             a server started with --mcp: its endpoint,
                          http://127.0.0.1:8080/_mcp, or /t/<tenant>/_mcp. The
                          tools are the server's
    --token-env <name>    the environment variable holding the token
                          default: FENEC_TOKEN

Claude Code: claude mcp add fenecdb -- fenec mcp --file notes.fenec --memory memories
";

pub fn main(args: &[String]) -> i32 {
    let mut file: Option<String> = None;
    let mut url: Option<String> = None;
    let mut read_only = false;
    let mut token_env = String::from("FENEC_TOKEN");
    // A local session's agent is the file's one caller: a bound would
    // only stop it.
    let mut opts = mcp::Options {
        calls_per_sec: 0,
        ..mcp::Options::default()
    };
    let mut local_flag: Option<&str> = None;
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        let mut value = || -> Option<String> {
            i += 1;
            args.get(i).cloned()
        };
        let ok = match flag {
            "-h" | "--help" => {
                print!("{USAGE}");
                return 0;
            }
            "--file" => value().map(|v| file = Some(v)).is_some(),
            "--url" => value().map(|v| url = Some(v)).is_some(),
            "--token-env" => value().map(|v| token_env = v).is_some(),
            "--read-only" => {
                read_only = true;
                true
            }
            "--write" => {
                local_flag = Some("--write");
                opts.write = true;
                true
            }
            "--memory" => {
                local_flag = Some("--memory");
                value().map(|v| opts.memory = Some(v)).is_some()
            }
            "--max-rows" => {
                local_flag = Some("--max-rows");
                value()
                    .and_then(|v| v.parse::<usize>().ok().filter(|n| *n > 0))
                    .map(|n| opts.max_rows = n.min(10_000))
                    .is_some()
            }
            "--max-bytes" => {
                local_flag = Some("--max-bytes");
                value()
                    .and_then(|v| fenec_http::quota::size(&v).filter(|n| *n > 0))
                    .map(|n| opts.max_bytes = n as usize)
                    .is_some()
            }
            other => {
                eprintln!("fenec mcp: unknown option `{other}`\n\n{USAGE}");
                return 2;
            }
        };
        if !ok {
            eprintln!("fenec mcp: {flag} expects a value\n\n{USAGE}");
            return 2;
        }
        i += 1;
    }
    if let Some(m) = &opts.memory {
        let ok = m
            .chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
            && m.chars().all(|c| c.is_alphanumeric() || c == '_');
        if !ok {
            eprintln!("fenec mcp: --memory names a collection, and `{m}` is not a name");
            return 2;
        }
    }
    match (file, url) {
        (Some(path), None) => local(&path, read_only, opts),
        (None, Some(url)) => {
            if let Some(f) = local_flag {
                eprintln!(
                    "fenec mcp: {f} is the server's to set (fenec-server --mcp ...): over --url the tools are its own"
                );
                return 2;
            }
            remote(&url, &token_env)
        }
        _ => {
            eprintln!("fenec mcp: give --file or --url\n\n{USAGE}");
            2
        }
    }
}

fn local(path: &str, read_only: bool, opts: mcp::Options) -> i32 {
    if read_only && opts.write {
        eprintln!("fenec mcp: --write and --read-only: pick one");
        return 2;
    }
    let lock = match read_only {
        true => None,
        false => match fenec_abi::lock::hold(path) {
            Ok(l) => Some(l),
            Err(e) => {
                eprintln!("fenec mcp: could not open {path}: {e}");
                return 1;
            }
        },
    };
    let opened = match read_only {
        true => fenec_core::fs::open_read_only(path),
        false => fenec_core::fs::open(path),
    };
    let db = match opened {
        Ok(db) => Arc::new(RwLock::new(db)),
        Err(e) => {
            eprintln!("fenec mcp: could not open {path}: {e}");
            return 1;
        }
    };
    let cfg = fenec_http::Config {
        read_only,
        // An agent's write is answered once it is on the disk, as the
        // native library answers an app's.
        sync_on_write: !read_only,
        ..fenec_http::Config::default()
    };
    let who = fenec_http::access::Who::Full;
    let ctx = mcp::Ctx {
        db: &db,
        cfg: &cfg,
        who: &who,
        opts: &opts,
        run: mcp::Run::local(),
    };
    eprintln!(
        "fenec mcp {}: {path}{}, over stdio",
        fenec_core::VERSION,
        if read_only { ", read only" } else { "" }
    );
    let served = mcp::serve_lines(&ctx, io::stdin().lock(), io::stdout().lock());
    let mut code = match served {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("fenec mcp: {e}");
            1
        }
    };
    if !read_only {
        let mut g = db.write().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = g.sync() {
            eprintln!("fenec mcp: could not push the writes to disk: {e}");
            code = 1;
        }
    }
    drop(lock);
    code
}

fn remote(url: &str, token_env: &str) -> i32 {
    let token = std::env::var(token_env).ok().filter(|t| !t.is_empty());
    let up = match mcp::bridge::Upstream::parse(url, token) {
        Ok(up) => up,
        Err(e) => {
            eprintln!("fenec mcp: {e}");
            return 2;
        }
    };
    eprintln!(
        "fenec mcp {}: {url}{}, over stdio",
        fenec_core::VERSION,
        if up.token.is_some() {
            ""
        } else {
            " with no token"
        }
    );
    match mcp::bridge::bridge(&up, io::stdin().lock(), io::stdout().lock()) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("fenec mcp: {e}");
            1
        }
    }
}
