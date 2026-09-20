//! `fenec-pg` -- serves fenecdb over the PostgreSQL protocol.
//!
//! ```text
//! fenec-pg [--listen 127.0.0.1:5433] [--file data.fenec] [--password secret]
//! psql -h 127.0.0.1 -p 5433 -U fenec
//! ```

use fenec_core::prelude::*;
use fenec_pg::client::{Client, Url};
use fenec_pg::server::{Auth, SyncPolicy};
use fenec_pg::{Config, PgPlugin, Server};
use std::sync::{Arc, RwLock};
use std::time::Duration;

const USAGE: &str = "\
usage: fenec-pg [options]

  -l, --listen <address>    default 127.0.0.1:5433
  -f, --file <path>         persistent fenecdb file (in-memory when absent)

  -W, --password <password> turn on password authentication
      --password-file <path> read the password from a file (argv shows up in `ps`)
      --auth <method>       scram | cleartext        default: scram
  -U, --user <name>         accept only this user name

      --sync <policy>       off | always | <ms>      default: 250
                            writes are buffered; this policy decides when
                            they reach the disk
      --no-checkpoint       do not write a checkpoint on shutdown. The
                            default is to write one: the HNSW graph lands in
                            the file and the next open does not rebuild it
                            (9.9 s -> 110 ms at 100k x 128). It lengthens
                            shutdown and peaks memory at ~3x the file
      --max-connections <n> ceiling on concurrent connections (0 = unlimited)
                            default: 100. Every connection is a thread
      --idle-timeout <s>    close a session silent for this long (0 = off)
      --max-message <MiB>   ceiling of a single protocol message  default: 64
      --max-memory <MiB>    data footprint ceiling (0 = off, the default).
                            Above it, writes stop with 53200; reads, `del`
                            and `compact` keep working. A third of the
                            container memory limit is a good start:
                            `compact` peaks at ~3x the file
      --insecure            allow listening without auth on a non-loopback address

      --http <address>      also open the HTTP/JSON endpoint (e.g. 127.0.0.1:8080).
                            A second listener in the same process: one process
                            writes one file, and the sync and checkpoint
                            policies are shared
      --http-token <value>  require `Authorization: Bearer <value>` for HTTP
      --http-cors <origin>  `Access-Control-Allow-Origin` (e.g. * or
                            https://example.com). Without it, no CORS header
      --http-read-only      turn off writes over HTTP (the pg path is unaffected)
      --http-max-streams <n>  ceiling on concurrent subscriptions (0 = unlimited)
                            default: 64. Subscriptions (`GET /<name>/changes`)
                            are long lived and each holds a thread, so they
                            are counted apart from --max-connections
      --http-keepalive <s>  keep-alive interval while a subscription is silent
                            default: 20. Proxies drop silent connections
      --changes <n>         entry count of the change ring  default: 4096.
                            It decides how far behind a subscriber may fall:
                            on overflow that subscriber is reseeded. At 100
                            writes per second, 4096 is a ~40 second window.
                            24 bytes per entry

      --ping                connect to the server and exit: 0 = up, 1 = not.
                            For health checks; `--listen`, `--user` and the
                            password options pick the target

The password is also read from the FENECPG_PASSWORD environment variable.
fenec-pg does not speak TLS: put it behind a TLS terminator such as
stunnel/nginx-stream before using it on an open network.
";

fn fail(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(2);
}

/// Health check: connects, authenticates, waits for `ReadyForQuery` and
/// closes -- the same depth as `pg_isready`.
///
/// It deliberately runs no query: every query takes the database lock first,
/// so during a long `compact` the probe would wait too and a healthy server
/// would look dead.
fn health_check(addr: &str, user: Option<&str>, password: Option<&str>) -> i32 {
    let (host, port) = match addr.rsplit_once(':') {
        Some((h, p)) => match p.parse::<u16>() {
            Ok(p) => (h.trim_matches(['[', ']']), p),
            Err(_) => {
                eprintln!("could not parse the port in the address: {addr}");
                return 1;
            }
        },
        None => {
            eprintln!("the address must be in `host:port` form: {addr}");
            return 1;
        }
    };
    // If the listen address is 0.0.0.0 / [::] we do not connect there; the
    // server is listening on loopback as well.
    let host = match host {
        "0.0.0.0" | "" => "127.0.0.1",
        "::" => "::1",
        h => h,
    };
    let url = Url {
        user: user.unwrap_or("fenec").to_string(),
        password: password.map(String::from),
        host: host.to_string(),
        port,
        database: "fenec".into(),
    };
    match Client::connect(&url) {
        Ok(_) => 0,
        Err(e) => {
            eprintln!("ping failed: {e}");
            1
        }
    }
}

fn main() {
    let mut cfg = Config::default();
    let mut file: Option<String> = None;
    let mut password: Option<String> = std::env::var("FENECPG_PASSWORD").ok();
    let mut method = "scram".to_string();
    let mut ping = false;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut http: Option<String> = None;
    let mut http_cfg = fenec_http::Config::default();

    let mut i = 0;
    let next = |i: &mut usize, flag: &str| -> String {
        *i += 1;
        match args.get(*i) {
            Some(v) => v.clone(),
            None => fail(&format!("{flag} expects a value")),
        }
    };
    while i < args.len() {
        match args[i].as_str() {
            "--listen" | "-l" => cfg.addr = next(&mut i, "--listen"),
            "--file" | "-f" => file = Some(next(&mut i, "--file")),
            "--password" | "-W" => password = Some(next(&mut i, "--password")),
            "--password-file" => {
                let path = next(&mut i, "--password-file");
                match std::fs::read_to_string(&path) {
                    Ok(s) => password = Some(s.trim_end_matches(['\n', '\r']).to_string()),
                    Err(e) => fail(&format!("could not read {path}: {e}")),
                }
            }
            "--auth" => method = next(&mut i, "--auth"),
            "--user" | "-U" => cfg.user = Some(next(&mut i, "--user")),
            "--sync" => match SyncPolicy::parse(&next(&mut i, "--sync")) {
                Ok(p) => cfg.sync = p,
                Err(e) => fail(&e),
            },
            "--no-checkpoint" => cfg.checkpoint_on_exit = false,
            "--max-connections" => {
                let v = next(&mut i, "--max-connections");
                cfg.max_connections = v.parse().unwrap_or_else(|_| {
                    fail(&format!("--max-connections expects a number, got `{v}`"))
                })
            }
            "--idle-timeout" => {
                let v = next(&mut i, "--idle-timeout");
                let secs: u64 = v.parse().unwrap_or_else(|_| {
                    fail(&format!("--idle-timeout expects seconds, got `{v}`"))
                });
                cfg.idle_timeout = (secs > 0).then(|| Duration::from_secs(secs));
            }
            "--max-memory" => {
                let v = next(&mut i, "--max-memory");
                let mib: usize = v
                    .parse()
                    .unwrap_or_else(|_| fail(&format!("--max-memory expects MiB, got `{v}`")));
                cfg.max_memory = mib << 20;
            }
            "--max-message" => {
                let v = next(&mut i, "--max-message");
                let mib: usize = v
                    .parse()
                    .unwrap_or_else(|_| fail(&format!("--max-message expects MiB, got `{v}`")));
                if mib == 0 {
                    fail("--max-message cannot be zero");
                }
                cfg.max_message = mib << 20;
            }
            "--http" => http = Some(next(&mut i, "--http")),
            "--http-token" => http_cfg.token = Some(next(&mut i, "--http-token")),
            "--http-cors" => http_cfg.cors = Some(next(&mut i, "--http-cors")),
            "--http-read-only" => http_cfg.read_only = true,
            "--http-max-streams" => {
                let v = next(&mut i, "--http-max-streams");
                http_cfg.max_streams = v.parse().unwrap_or_else(|_| {
                    fail(&format!("--http-max-streams expects a number, got `{v}`"))
                })
            }
            "--http-keepalive" => {
                let v = next(&mut i, "--http-keepalive");
                let secs: u64 = v.parse().unwrap_or_else(|_| {
                    fail(&format!("--http-keepalive expects seconds, got `{v}`"))
                });
                if secs == 0 {
                    fail("--http-keepalive cannot be zero");
                }
                http_cfg.stream_keepalive = Duration::from_secs(secs);
            }
            "--changes" => {
                let v = next(&mut i, "--changes");
                http_cfg.change_capacity = v
                    .parse()
                    .unwrap_or_else(|_| fail(&format!("--changes expects a number, got `{v}`")))
            }
            "--ping" => ping = true,
            "--insecure" => cfg.insecure = true,
            "--help" | "-h" => {
                eprintln!("fenec-pg {}\n\n{USAGE}", fenec_core::VERSION);
                return;
            }
            other => fail(&format!("unknown option: {other}\n\n{USAGE}")),
        }
        i += 1;
    }

    if ping {
        std::process::exit(health_check(
            &cfg.addr,
            cfg.user.as_deref(),
            password.as_deref(),
        ));
    }

    cfg.auth = match &password {
        Some(pw) if pw.is_empty() => fail("the password cannot be empty"),
        Some(pw) => match Auth::parse(&method, pw) {
            Ok(a) => a,
            Err(e) => fail(&e),
        },
        None => Auth::Trust,
    };

    let mut db = match &file {
        Some(path) => match fenec_core::fs::open(path) {
            Ok(db) => {
                eprintln!("opened: {path}");
                db
            }
            Err(e) => {
                eprintln!("could not open {path}: {e}");
                std::process::exit(1);
            }
        },
        None => {
            if cfg.sync != SyncPolicy::Off {
                // Syncing makes no sense for an in-memory database.
                cfg.sync = SyncPolicy::Off;
            }
            // Nor does a checkpoint: there is no file to write to, but the
            // image would still be built in memory.
            cfg.checkpoint_on_exit = false;
            Database::new()
        }
    };

    if let Err(e) = db.install_plugin(&PgPlugin) {
        eprintln!("could not load the plugin: {e}");
        std::process::exit(1);
    }

    let shared = Arc::new(RwLock::new(db));

    // The HTTP endpoint shares the same database: as a separate binary it
    // would open the same file from two processes and corrupt it (fenecdb is
    // single-writer).
    if let Some(addr) = http {
        http_cfg.addr = addr;
        http_cfg.insecure = cfg.insecure;
        http_cfg.max_connections = cfg.max_connections;
        http_cfg.idle_timeout = cfg.idle_timeout;
        // `--sync always` must hold for HTTP writes too.
        http_cfg.sync_on_write = cfg.sync == SyncPolicy::Always;
        let http_server = fenec_http::Server::new(Arc::clone(&shared), http_cfg);
        let listener = match http_server.bind() {
            Ok(l) => l,
            Err(e) => {
                eprintln!("could not open the HTTP endpoint: {e}");
                std::process::exit(1);
            }
        };
        std::thread::Builder::new()
            .name("fenec-http".into())
            .spawn(move || {
                if let Err(e) = http_server.serve_on(listener) {
                    eprintln!("HTTP server error: {e}");
                }
            })
            .unwrap_or_else(|e| fail(&format!("could not start the HTTP thread: {e}")));
    }

    let server = Server::new(shared, cfg);
    if let Err(e) = server.serve() {
        eprintln!("server error: {e}");
        std::process::exit(1);
    }
}
