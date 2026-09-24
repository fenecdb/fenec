//! `fenec-pg` -- serves fenecdb over the PostgreSQL protocol.
//!
//! ```text
//! fenec-pg [--listen 127.0.0.1:5433] [--file data.fenec] [--password secret]
//! psql -h 127.0.0.1 -p 5433 -U fenec
//! ```

use fenec_core::prelude::*;
use fenec_http::replication::{self, Follower, Replication};
use fenec_http::tenants::{Refused, Tenants};
use fenec_pg::client::{Client, Url};
use fenec_pg::server::{self, Auth, SyncPolicy};
use fenec_pg::{Config, PgPlugin, Server};
use std::sync::{Arc, RwLock};
use std::time::Duration;

const USAGE: &str = "\
usage: fenec-pg [options]

  -l, --listen <address>    default 127.0.0.1:5433
  -f, --file <path>         persistent fenecdb file (in-memory when absent)
      --dir <path>          one file per tenant in this directory, served over
                            HTTP under /t/<tenant>/. Needs --http. With
                            --listen it serves the pg wire as well, where the
                            database in the startup packet is the tenant
                            (psql postgres://host:port/acme)
      --admin-token <value> token for /_admin/ (--dir only): create, delete,
                            freeze and move tenants. Without it, off
      --idle-close <s>      close a tenant untouched for this long  default: 300
                            (0 = never). The next request reopens it
      --no-mmap             read the file into memory instead of mapping it.
                            Mapping leaves the documents in the file and
                            holds only what is derived from them; read it
                            instead over a network file system, or to have
                            --max-memory cover the data as well

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
                            `compact` peaks at ~3x the file. With --dir it
                            covers the open tenants together, and opening one
                            more over it closes the idle ones first
      --insecure            allow listening without auth on a non-loopback address

      --http <address>      also open the HTTP/JSON endpoint (e.g. 127.0.0.1:8080).
                            A second listener in the same process: one process
                            writes one file, and the sync and checkpoint
                            policies are shared
      --http-token <value>  require `Authorization: Bearer <value>` for HTTP
      --jwt-secret <value>  also take HS256 JSON Web Tokens signed with this,
                            each held to --policy: which collections, which
                            rows (`where owner = $jwt.sub`). Also read from
                            FENEC_JWT_SECRET; at least 32 bytes
      --jwt-secret-file <path>  the secret from a file
      --policy <path>       the rules a token is held to, one per line:
                            <collection|*> <read|write|read,write>
                            [where <filter>] [for <role>]
      --mint-token <claims> print a token for this JSON object of claims,
                            signed with the secret, and exit
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

      --slow-ms <ms>        log every statement that takes this long or longer,
                            with its text: pg and HTTP alike, from its arrival
                            to its answer. Off by default
      --metrics <address>   serve /_metrics, and nothing else, here -- for a
                            server with no --http. With --http, the HTTP
                            listener serves it too. Readable with
                            --http-token or --admin-token; a non-loopback
                            address wants one of them (or --insecure)

      --replication-token <value>  turn replication on: /_replication on the
                            HTTP listener feeds replicas the writes on this
                            file's disk, reports status, and promotes a
                            replica. With --dir every tenant has one of its
                            own under /t/<tenant>/_replication. Refuses
                            --sync off. Also read from
                            FENEC_REPLICATION_TOKEN
      --replica-of <url>    follow the primary at http://host:port and take
                            no write of its own (25006). With --dir it
                            follows that node's tenant of the same name, and
                            a failover promotes them one at a time
                            (POST /_admin/tenants/<t>/promote)
      --promote             open a replica's file to take writes: its history
                            forks here. A replica's file opens only with
                            --replica-of or this
      --replication-buffer <MiB>  writes kept for replicas that fall behind
                            default: 64. One further behind is sent an image

      --ping                connect to the server and exit: 0 = up, 1 = not.
                            For health checks; `--listen`, `--user` and the
                            password options pick the target

The password is also read from the FENECPG_PASSWORD environment variable.
fenec-pg does not speak TLS: put it behind a TLS terminator such as
stunnel/nginx-stream before using it on an open network.
";

fn fail(msg: &str) -> ! {
    fenec_http::log!("{msg}");
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
                fenec_http::log!("could not parse the port in the address: {addr}");
                return 1;
            }
        },
        None => {
            fenec_http::log!("the address must be in `host:port` form: {addr}");
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
            fenec_http::log!("ping failed: {e}");
            1
        }
    }
}

fn main() {
    let mut cfg = Config::default();
    let mut file: Option<String> = None;
    let mut dir: Option<String> = None;
    let mut mmap = true;
    // `--dir` opens the pg listener only when an address was named: a node
    // that serves tenants over HTTP alone should not take the default port.
    let mut listen_given = false;
    let mut idle_close = Duration::from_secs(300);
    let mut password: Option<String> = std::env::var("FENECPG_PASSWORD").ok();
    let mut method = "scram".to_string();
    let mut ping = false;
    let mut replication_token: Option<String> = std::env::var("FENEC_REPLICATION_TOKEN").ok();
    let mut replica_of: Option<String> = None;
    let mut promote = false;
    let mut replication_buffer = replication::DEFAULT_BUFFER;
    let mut jwt_secret: Option<String> = std::env::var("FENEC_JWT_SECRET").ok();
    let mut policy: Option<String> = None;
    let mut mint: Option<String> = None;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut http: Option<String> = None;
    let mut metrics: Option<String> = None;
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
            "--listen" | "-l" => {
                cfg.addr = next(&mut i, "--listen");
                listen_given = true;
            }
            "--file" | "-f" => file = Some(next(&mut i, "--file")),
            "--dir" => dir = Some(next(&mut i, "--dir")),
            "--admin-token" => http_cfg.admin_token = Some(next(&mut i, "--admin-token")),
            "--idle-close" => {
                let v = next(&mut i, "--idle-close");
                let secs: u64 = v
                    .parse()
                    .unwrap_or_else(|_| fail(&format!("--idle-close expects seconds, got `{v}`")));
                idle_close = Duration::from_secs(secs);
            }
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
            "--metrics" => metrics = Some(next(&mut i, "--metrics")),
            "--slow-ms" => {
                let v = next(&mut i, "--slow-ms");
                let ms: u64 = v.parse().unwrap_or_else(|_| {
                    fail(&format!("--slow-ms expects milliseconds, got `{v}`"))
                });
                fenec_http::metrics::set_slow(ms);
            }
            "--http-token" => http_cfg.token = Some(next(&mut i, "--http-token")),
            "--jwt-secret" => jwt_secret = Some(next(&mut i, "--jwt-secret")),
            "--jwt-secret-file" => {
                let path = next(&mut i, "--jwt-secret-file");
                match std::fs::read_to_string(&path) {
                    Ok(s) => jwt_secret = Some(s.trim_end_matches(['\n', '\r']).to_string()),
                    Err(e) => fail(&format!("could not read {path}: {e}")),
                }
            }
            "--policy" => {
                let path = next(&mut i, "--policy");
                match std::fs::read_to_string(&path) {
                    Ok(s) => policy = Some(s),
                    Err(e) => fail(&format!("could not read {path}: {e}")),
                }
            }
            "--mint-token" => mint = Some(next(&mut i, "--mint-token")),
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
            "--replication-token" => replication_token = Some(next(&mut i, "--replication-token")),
            "--replica-of" => {
                let url = next(&mut i, "--replica-of");
                if let Err(e) = fenec_http::replication::Upstream::check_node(&url) {
                    fail(&e);
                }
                replica_of = Some(url)
            }
            "--promote" => promote = true,
            "--replication-buffer" => {
                let v = next(&mut i, "--replication-buffer");
                let mib: usize = v.parse().unwrap_or_else(|_| {
                    fail(&format!("--replication-buffer expects MiB, got `{v}`"))
                });
                replication_buffer = mib << 20;
            }
            "--ping" => ping = true,
            "--no-mmap" => mmap = false,
            "--insecure" => cfg.insecure = true,
            "--help" | "-h" => {
                fenec_http::log!("fenec-pg {}\n\n{USAGE}", fenec_core::VERSION);
                return;
            }
            other => fail(&format!("unknown option: {other}\n\n{USAGE}")),
        }
        i += 1;
    }

    if let Some(claims) = mint {
        let secret = jwt_secret.unwrap_or_else(|| fail("--mint-token signs with --jwt-secret"));
        let access =
            fenec_http::access::Access::new(secret.as_bytes(), "").unwrap_or_else(|e| fail(&e));
        println!("{}", access.mint(&claims).unwrap_or_else(|e| fail(&e)));
        return;
    }
    match (jwt_secret, policy) {
        (Some(secret), Some(policy)) => {
            let access = fenec_http::access::Access::new(secret.as_bytes(), &policy)
                .unwrap_or_else(|e| fail(&e));
            http_cfg.access = Some(Arc::new(access));
        }
        (Some(_), None) => fail("--jwt-secret needs --policy: without rules a token reads nothing"),
        (None, Some(_)) => fail("--policy needs --jwt-secret: the rules are for tokens it signs"),
        (None, None) => {}
    }

    if ping {
        std::process::exit(health_check(
            &cfg.addr,
            cfg.user.as_deref(),
            password.as_deref(),
        ));
    }

    let replicating = replication_token.as_deref().is_some_and(|t| !t.is_empty());
    if replica_of.is_some() && !replicating {
        fail("--replica-of needs --replication-token: the primary asks for it");
    }
    if replica_of.is_some() && promote {
        fail("--replica-of follows a primary and --promote stops following: pick one");
    }
    if (replicating || promote) && file.is_none() && dir.is_none() {
        fail("replication works on a file or a directory of them: give --file or --dir");
    }
    if replicating && cfg.sync == SyncPolicy::Off {
        fail(
            "--sync off puts nothing on disk before shutdown, and a replica is sent only \
             what is on the primary's disk: use --sync always or --sync <ms>",
        );
    }
    if replicating && replica_of.is_none() && http.is_none() {
        fail("replicas are fed over HTTP: give --http <address>");
    }

    cfg.auth = match &password {
        Some(pw) if pw.is_empty() => fail("the password cannot be empty"),
        Some(pw) => match Auth::parse(&method, pw) {
            Ok(a) => a,
            Err(e) => fail(&e),
        },
        None => Auth::Trust,
    };

    if let Some(dir) = dir {
        if promote {
            fail(
                "a tenant is promoted one at a time, by the router: \
                 POST /_admin/tenants/<tenant>/promote",
            );
        }
        if replica_of.is_some() && !replicating {
            fail("--replica-of needs --replication-token: the node it follows asks for it");
        }
        if file.is_some() {
            fail(
                "--dir and --file are exclusive: one serves a file, the other a directory of them",
            );
        }
        let Some(addr) = http else {
            fail("--dir serves tenants over HTTP: give --http <address>");
        };
        if metrics.is_some() {
            fail("with --dir the HTTP listener serves /_metrics: --metrics is for a --file server");
        }
        http_cfg.addr = addr;
        http_cfg.insecure = cfg.insecure;
        http_cfg.max_connections = cfg.max_connections;
        http_cfg.idle_timeout = cfg.idle_timeout;
        http_cfg.sync_on_write = cfg.sync == SyncPolicy::Always;
        http_cfg.max_memory = cfg.max_memory;
        let repl = replication_token.filter(|_| replicating).map(|token| {
            fenec_http::tenants::Replicated {
                token,
                buffer: replication_buffer,
                upstream: replica_of.clone(),
                sync_on_write: cfg.sync == SyncPolicy::Always,
            }
        });
        serve_dir(&dir, http_cfg, cfg, idle_close, listen_given, mmap, repl);
    }

    let mut feed = None;
    let mut db = match &file {
        Some(path) => {
            let opened = if replicating {
                replication::open_serving(path, replication_buffer, mmap).map(|(db, f)| {
                    feed = Some(f);
                    db
                })
            } else {
                fenec_core::fs::open_serving(path, mmap, Box::new(Ok))
            };
            match opened {
                Ok(db) => {
                    fenec_http::log!("opened: {path}");
                    db
                }
                Err(e) => {
                    fenec_http::log!("could not open {path}: {e}");
                    std::process::exit(1);
                }
            }
        }
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
        fenec_http::log!("could not load the plugin: {e}");
        std::process::exit(1);
    }

    if let Some(path) = &file {
        if let Err(e) = replication::settle_history(
            &mut db,
            path,
            ("a replica's file", "writes"),
            replicating,
            replica_of.is_some(),
            promote,
        ) {
            fenec_http::log!("{e}");
            std::process::exit(1);
        }
    }

    let shared = Arc::new(RwLock::new(db));
    // What the open left out of the graphs is linked beside the queries: the
    // port opens in the time the documents take to read. The graphs are kept
    // in the file as they change, so a crash leaves only what came after
    // the last of them to link.
    if let Some(path) = &file {
        fenec_http::link::beside(path, &shared);
        fenec_http::link::keep(path, &shared);
    }

    // The follower applies the primary's writes; this server's own feed
    // passes them on to replicas of its own.
    let follower = replica_of.as_ref().map(|url| {
        let f = Follower::new(
            url,
            replication_token.clone().unwrap_or_default(),
            Arc::clone(&shared),
            feed.clone(),
            cfg.sync == SyncPolicy::Always,
        )
        .unwrap_or_else(|e| fail(&e));
        f.start("fenec-replica".into())
            .unwrap_or_else(|e| fail(&format!("could not start the replica thread: {e}")));
        fenec_http::log!("following: {url}");
        f
    });
    let repl = replication_token
        .filter(|_| replicating)
        .map(|token| Replication::new(token, feed.clone(), follower));

    // Before the HTTP thread announces its listener, as `Server::serve_on`
    // does before its own: the flag a signal sets waits for the syncer.
    server::install_signal_handlers();

    // `--metrics`: /_metrics alone on a listener of its own, readable with
    // the tokens the HTTP endpoint takes.
    if let Some(addr) = metrics {
        let mcfg = fenec_http::Config {
            addr,
            token: http_cfg.token.clone(),
            admin_token: http_cfg.admin_token.clone(),
            insecure: cfg.insecure,
            idle_timeout: cfg.idle_timeout,
            ..fenec_http::Config::default()
        };
        let server = fenec_http::Server::metrics_only(Arc::clone(&shared), repl.clone(), mcfg);
        let listener = server
            .bind()
            .unwrap_or_else(|e| fail(&format!("could not open the metrics endpoint: {e}")));
        std::thread::Builder::new()
            .name("fenec-metrics".into())
            .spawn(move || {
                if let Err(e) = server.serve_on(listener) {
                    fenec_http::log!("metrics server error: {e}");
                }
            })
            .unwrap_or_else(|e| fail(&format!("could not start the metrics thread: {e}")));
    }

    // The HTTP endpoint shares the same database: as a separate binary it
    // would open the same file from two processes and corrupt it (fenecdb is
    // single-writer).
    if let Some(addr) = http {
        http_cfg.addr = addr;
        http_cfg.insecure = cfg.insecure;
        http_cfg.max_connections = cfg.max_connections;
        http_cfg.idle_timeout = cfg.idle_timeout;
        // `--sync always` must hold for HTTP writes too, and so must
        // `--max-memory`.
        http_cfg.sync_on_write = cfg.sync == SyncPolicy::Always;
        http_cfg.max_memory = cfg.max_memory;
        let mut http_server = fenec_http::Server::new(Arc::clone(&shared), http_cfg);
        if let Some(repl) = repl {
            http_server = http_server.with_replication(repl);
        }
        let listener = match http_server.bind() {
            Ok(l) => l,
            Err(e) => {
                fenec_http::log!("could not open the HTTP endpoint: {e}");
                std::process::exit(1);
            }
        };
        std::thread::Builder::new()
            .name("fenec-http".into())
            .spawn(move || {
                if let Err(e) = http_server.serve_on(listener) {
                    fenec_http::log!("HTTP server error: {e}");
                }
            })
            .unwrap_or_else(|e| fail(&format!("could not start the HTTP thread: {e}")));
    }

    let server = Server::new(shared, cfg);
    if let Err(e) = server.serve() {
        fenec_http::log!("server error: {e}");
        std::process::exit(1);
    }
}

/// `--dir`: the HTTP listener over a directory of tenants, and on this
/// thread the syncer that a single file gets from the pg server -- periodic
/// sync, idle close, and on the shutdown signal a final sync and checkpoint
/// of every open tenant.
fn serve_dir(
    dir: &str,
    http_cfg: fenec_http::Config,
    cfg: Config,
    idle_close: Duration,
    pg: bool,
    mmap: bool,
    repl: Option<fenec_http::tenants::Replicated>,
) -> ! {
    let tenants = match Tenants::new(dir) {
        Ok(t) => t,
        Err(e) => fail(&format!("could not use {dir}: {e}")),
    };
    let mut tenants = tenants
        .with_setup(|db| db.install_plugin(&PgPlugin))
        .with_change_capacity(http_cfg.change_capacity)
        .with_max_memory(cfg.max_memory)
        .with_checkpoint(cfg.checkpoint_on_exit)
        .with_mmap(mmap);
    let follows = repl.as_ref().and_then(|r| r.upstream.clone());
    if let Some(r) = repl {
        tenants = tenants.with_replication(r);
    }
    let tenants = Arc::new(tenants);
    fenec_http::log!(
        "serving tenants from: {dir}  ({} on disk)",
        tenants.names().len()
    );
    // A replica node's tenants have to be open to follow: nothing else
    // touches them there, and a follower that is not running is a replica
    // falling behind. Opening one starts its follower.
    if let Some(url) = &follows {
        fenec_http::log!("following the tenants of: {url}");
        for name in tenants.names() {
            if let Err(Refused(status, msg)) = tenants.get(&name) {
                fenec_http::log!("tenant `{name}` did not open ({status}): {msg}");
            }
        }
    }

    // The pg listener, when an address was named: there the database in the
    // startup packet is the tenant (`psql postgres://host:port/acme`). It
    // runs no syncer -- the loop below is this node's, over every open
    // tenant -- and the same --password guards it as guards a file server.
    let sync = cfg.sync;
    if pg {
        let server = Server::with_tenants(Arc::clone(&tenants), cfg);
        let listener = match server.bind() {
            Ok(l) => l,
            Err(e) => fail(&format!("could not open the pg endpoint: {e}")),
        };
        std::thread::Builder::new()
            .name("fenec-pg".into())
            .spawn(move || {
                if let Err(e) = server.serve_on(listener) {
                    fenec_http::log!("pg server error: {e}");
                    std::process::exit(1);
                }
            })
            .unwrap_or_else(|e| fail(&format!("could not start the pg thread: {e}")));
    }

    let http_server = fenec_http::Server::with_tenants(Arc::clone(&tenants), http_cfg);
    let listener = match http_server.bind() {
        Ok(l) => l,
        Err(e) => fail(&format!("could not open the HTTP endpoint: {e}")),
    };
    // Before the thread that announces the listener, for the reason
    // `Server::serve_on` gives: once the line is out, SIGTERM must sync.
    server::install_signal_handlers();
    std::thread::Builder::new()
        .name("fenec-http".into())
        .spawn(move || {
            if let Err(e) = http_server.serve_on(listener) {
                fenec_http::log!("HTTP server error: {e}");
                std::process::exit(1);
            }
        })
        .unwrap_or_else(|e| fail(&format!("could not start the HTTP thread: {e}")));

    let tick = match sync {
        SyncPolicy::Interval(d) if !d.is_zero() => d,
        _ => Duration::from_millis(200),
    };
    loop {
        std::thread::sleep(tick);
        if server::shutdown_requested() {
            // The write locks come back held: nothing is accepted between
            // the last sync and exit.
            let open = tenants.shutdown();
            fenec_http::log!("\nshutting down: {open} open tenant(s) synced");
            std::process::exit(0);
        }
        if matches!(sync, SyncPolicy::Interval(_)) {
            tenants.sync_dirty();
        }
        if !idle_close.is_zero() {
            tenants.close_idle(idle_close);
        }
    }
}
