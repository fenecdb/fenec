//! `fenec-server` -- serves a fenecdb file, or a directory of them, over HTTP.
//!
//! ```text
//! fenec-server [--http 127.0.0.1:8080] [--file data.fenec] [--http-token secret]
//! curl -d '{"q": "list"}' http://127.0.0.1:8080/query
//! ```

use fenec_core::prelude::*;
use fenec_http::replication::{self, Follower, Replication};
use fenec_http::tenants::{Refused, Tenants};
use fenec_server::durability::{self, SyncPolicy};
use std::io::{Read, Write};
use std::sync::{Arc, RwLock};
use std::time::Duration;

const USAGE: &str = "\
usage: fenec-server [options]

  -f, --file <path>         persistent fenecdb file (in-memory when absent)
      --http <address>      where the HTTP/JSON endpoint listens
                            default: 127.0.0.1:8080
      --dir <path>          one file per tenant in this directory, served
                            under /t/<tenant>/
      --admin-token <value> token for /_admin/ (--dir only): create, delete,
                            freeze and move tenants. Without it, off
      --idle-close <s>      close a tenant untouched for this long  default: 300
                            (0 = never). The next request reopens it
      --lease               write a tenant only while the router's lease names
                            it and has not lapsed (--dir), starting with none:
                            what lets fenec-shard --auto-failover promote the
                            node's tenants elsewhere without two primaries
      --no-mmap             read the file into memory instead of mapping it.
                            Mapping leaves the documents in the file and
                            holds only what is derived from them; read it
                            instead over a network file system, or to have
                            --max-memory cover the data as well
      --warm <all|off|list> build the hash, text, ordered and sparse indexes
                            after the open, one at a time beside the
                            queries, rather than on the first read of each:
                            all, off, or collections and collection.fields
                            by commas. A tenant's as it opens (--dir)

      --sync <policy>       off | always | <ms>      default: 250
                            writes are buffered; this policy decides when
                            they reach the disk
      --auto-compact <ratio|off>  compact a file on its own once this share
                            of it is dead -- versions updates and deletes
                            left behind -- and at least 64 MB, beside the
                            queries: the write lock is taken only to put the
                            new file in place. Every file served is looked
                            at every 5 s, tenants and replicas too
                            default: 0.5 (the file stays under twice its data)
      --no-checkpoint       do not write a checkpoint on shutdown. The
                            default is to write one: the HNSW graph lands in
                            the file and the next open does not rebuild it
                            (4.4 s -> 9 ms at 100k x 128). It lengthens
                            shutdown and peaks memory at ~3x the file
      --max-connections <n> ceiling on concurrent connections (0 = unlimited)
                            default: 100. Every connection is a thread
      --idle-timeout <s>    close a keep-alive connection silent for this
                            long (0 = never)  default: 60
      --max-memory <MiB>    data footprint ceiling (0 = off, the default).
                            Above it, writes stop with 507; reads, `del`
                            and `compact` keep working. A third of the
                            container memory limit is a good start:
                            `compact` peaks at ~3x the file. With --dir it
                            covers the open tenants together, and opening one
                            more over it closes the idle ones first
      --insecure            allow listening without a token on a non-loopback
                            address

      --http-token <value>  require `Authorization: Bearer <value>`. Also read
                            from FENEC_HTTP_TOKEN: argv shows up in `ps`
      --jwt-secret <value>  also take HS256 JSON Web Tokens signed with this,
                            each held to --policy: which collections, which
                            rows (`where owner = $jwt.sub`). Also read from
                            FENEC_JWT_SECRET; at least 32 bytes
      --jwt-secret-file <path>  the secret from a file
      --jwt-keys <path>     instead of a secret, the keys of a JWKS file
                            (its `keys` list): `oct` keys take HS256 tokens,
                            `RSA` keys an identity provider's RS256 ones, a
                            token's `kid` names its key. Read again as the
                            file changes, which is how keys rotate. Also read
                            from FENEC_JWT_KEYS
      --jwt-require-exp <on|off>  refuse a token with no `exp` claim, which
                            would be good for ever  default: on
                            (--mint-token gives one an hour)
      --jwt-max-age <s>     refuse a token whose `exp` lies further ahead
                            than this (0 = no bound, the default)
      --jwt-tenant-claim <name>  with --dir, the claim naming the tenant a
                            token is for: it reaches /t/<t>/ only when the
                            claim names <t> (a text, or a list holding it)
                            default: tenant
      --jwt-unbound-tenants with --dir, take a token naming no tenant for
                            every tenant. Without it such a token is refused
                            (403): a policy's `owner = $jwt.sub` matches the
                            same user in every tenant's file
      --policy <path>       the rules a token is held to, one per line:
                            <collection|*> <grants> [where <filter>]
                            [for <role>], the grants read, insert, update,
                            delete and write (all three) joined by commas;
                            `<collection> append-only` refuses every
                            update and delete a token asks for there
      --mint-token <claims> print a token for this JSON object of claims,
                            signed with the secret or the first `oct` key,
                            and exit
      --http-cors <origin>  `Access-Control-Allow-Origin` (e.g. * or
                            https://example.com). Without it, no CORS header
      --http-read-only      turn off writes
      --studio              serve fenec studio, the admin pages, at
                            /_studio/: collections, rows and edits in a
                            browser, with the token pasted into the page and
                            no authority of their own. Off by default; keep
                            it on a private network or behind your own auth
      --studio-connect <origin>  an origin the studio's pages may also send
                            requests to: a router (https://router:8080)
      --idempotency-ttl <s>  how long a write's Idempotency-Key and answer
                            are kept, in seconds. default: 86400
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
                            with its text, from its arrival to its answer,
                            as a JSON line on stderr; 0 logs every one.
                            Off by default
      --audit <path>        append a JSON line to this file for each request
                            refused for its token, each change of the schema
                            and each admin request. Off by default
      --auth-delay <ms>     how long a refused token waits before it is
                            answered, doubled for each refusal from the same
                            address within a minute, 5 s at most. 0 turns it
                            off. default: 100
      --metrics <address>   also serve /_metrics, and nothing else, here --
                            an address apart from the data's. Readable with
                            --http-token or --admin-token; a non-loopback
                            address wants one of them (or --insecure)

      --replication-token <value>  turn replication on: /_replication feeds
                            replicas the writes on this file's disk, reports
                            status, and promotes a replica. With --dir every
                            tenant has one of its own under
                            /t/<tenant>/_replication. Refuses --sync off.
                            Also read from FENEC_REPLICATION_TOKEN
      --replica-of <url>    follow the primary at http://host:port and take
                            no write of its own (403). With --dir it
                            follows that node's tenant of the same name, and
                            a failover promotes them one at a time
                            (POST /_admin/tenants/<t>/promote)
      --promote             open a replica's file to take writes: its history
                            forks here. A replica's file opens only with
                            --replica-of or this
      --replication-buffer <MiB>  writes kept for replicas that fall behind
                            default: 64. One further behind is sent an image
      --cdc                 keep the writes on disk for GET /_changes (change
                            data capture), as many as --replication-buffer
                            holds, with no replicas. With --replication-token
                            it is on already

      --follow <url>        mirror a table of the PostgreSQL server at
                            postgres://user@host/db into --file, and serve it:
                            its copy when there is no whole one, then its
                            changes as they commit, through a logical
                            replication slot. The collection takes no write
                            but the follower's
      --follow-table <name> the table (required with --follow)
      --follow-into <name>  the collection  default: the table's name
      --follow-slot <name>, --follow-publication <name>
                            made if missing  default: fenec_<collection>
      --follow-index, --follow-vector, --follow-cast, --follow-id,
      --follow-where, --follow-batch
                            as `fenec import`'s --index and the rest: how the
                            rows become documents, the copy's and the
                            changes' alike

      --ping                ask the server at --http for GET /_health and
                            exit: 0 = up, 1 = not. For health checks

fenec-server does not speak TLS: put it behind a TLS terminator such as
nginx or Caddy before using it on an open network.
";

fn fail(msg: &str) -> ! {
    fenec_http::log!("{msg}");
    std::process::exit(2);
}

/// Health check: `GET /_health` on the HTTP listener, which answers with no
/// token and takes no lock -- during a long `compact` a probe that queried
/// would wait too, and a healthy server would look dead.
fn health_check(addr: &str) -> i32 {
    // A server listening on every address answers on loopback as well.
    let addr = match addr.rsplit_once(':') {
        Some(("0.0.0.0" | "", port)) => format!("127.0.0.1:{port}"),
        Some(("[::]", port)) => format!("[::1]:{port}"),
        _ => addr.to_string(),
    };
    let asked = (|| -> std::io::Result<String> {
        let mut s = std::net::TcpStream::connect(&addr)?;
        s.set_read_timeout(Some(Duration::from_secs(3)))?;
        s.write_all(b"GET /_health HTTP/1.1\r\nHost: fenec\r\nConnection: close\r\n\r\n")?;
        let mut out = String::new();
        s.read_to_string(&mut out)?;
        Ok(out)
    })();
    match asked {
        Ok(out) if out.starts_with("HTTP/1.1 200") => 0,
        Ok(out) => {
            let line = out.lines().next().unwrap_or("no answer");
            fenec_http::log!("ping failed: {line}");
            1
        }
        Err(e) => {
            fenec_http::log!("ping failed: {e}");
            1
        }
    }
}

fn main() {
    let mut sync = SyncPolicy::Interval(Duration::from_millis(250));
    let mut checkpoint = true;
    let mut file: Option<String> = None;
    let mut dir: Option<String> = None;
    let mut mmap = true;
    let mut lease = false;
    // `--warm`: None until given; a file is warmed whole unless told off,
    // a node's tenants only when told.
    let mut warm: Option<Option<fenec_http::warm::Warm>> = None;
    let mut idle_close = Duration::from_secs(300);
    let mut ping = false;
    let mut replication_token: Option<String> = std::env::var("FENEC_REPLICATION_TOKEN").ok();
    let mut replica_of: Option<String> = None;
    let mut promote = false;
    let mut cdc = false;
    let mut replication_buffer = replication::DEFAULT_BUFFER;
    let mut jwt_secret: Option<String> = std::env::var("FENEC_JWT_SECRET").ok();
    let mut jwt_keys: Option<String> = std::env::var("FENEC_JWT_KEYS").ok();
    let mut policy: Option<String> = None;
    let mut mint: Option<String> = None;
    let mut demands = fenec_http::access::Demands::default();
    let mut follow_url: Option<String> = None;
    let mut follow_table: Option<String> = None;
    let mut follow_into: Option<String> = None;
    let mut follow_slot: Option<String> = None;
    let mut follow_publication: Option<String> = None;
    let mut follow_opts = fenec_import::Options::new("");
    let mut follow_named = false;
    let mut studio = false;
    let mut studio_connect: Option<String> = None;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut metrics: Option<String> = None;
    let mut http_cfg = fenec_http::Config {
        token: std::env::var("FENEC_HTTP_TOKEN")
            .ok()
            .filter(|t| !t.is_empty()),
        ..fenec_http::Config::default()
    };

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
            "--file" | "-f" => file = Some(next(&mut i, "--file")),
            "--http" => http_cfg.addr = next(&mut i, "--http"),
            "--dir" => dir = Some(next(&mut i, "--dir")),
            "--admin-token" => http_cfg.admin_token = Some(next(&mut i, "--admin-token")),
            "--idle-close" => {
                let v = next(&mut i, "--idle-close");
                let secs: u64 = v
                    .parse()
                    .unwrap_or_else(|_| fail(&format!("--idle-close expects seconds, got `{v}`")));
                idle_close = Duration::from_secs(secs);
            }
            "--sync" => match SyncPolicy::parse(&next(&mut i, "--sync")) {
                Ok(p) => sync = p,
                Err(e) => fail(&e),
            },
            "--no-checkpoint" => checkpoint = false,
            "--max-connections" => {
                let v = next(&mut i, "--max-connections");
                http_cfg.max_connections = v.parse().unwrap_or_else(|_| {
                    fail(&format!("--max-connections expects a number, got `{v}`"))
                })
            }
            "--idle-timeout" => {
                let v = next(&mut i, "--idle-timeout");
                let secs: u64 = v.parse().unwrap_or_else(|_| {
                    fail(&format!("--idle-timeout expects seconds, got `{v}`"))
                });
                http_cfg.idle_timeout = (secs > 0).then(|| Duration::from_secs(secs));
            }
            "--max-memory" => {
                let v = next(&mut i, "--max-memory");
                let mib: usize = v
                    .parse()
                    .unwrap_or_else(|_| fail(&format!("--max-memory expects MiB, got `{v}`")));
                http_cfg.max_memory = mib << 20;
            }
            "--metrics" => metrics = Some(next(&mut i, "--metrics")),
            "--audit" => {
                let path = next(&mut i, "--audit");
                if let Err(e) = fenec_http::audit::open(std::path::Path::new(&path)) {
                    fail(&format!("could not open {path}: {e}"));
                }
            }
            "--auth-delay" => {
                let v = next(&mut i, "--auth-delay");
                let ms: u64 = v.parse().unwrap_or_else(|_| {
                    fail(&format!("--auth-delay expects milliseconds, got `{v}`"))
                });
                fenec_http::audit::set_delay(ms);
            }
            "--slow-ms" => {
                let v = next(&mut i, "--slow-ms");
                let ms: u64 = v.parse().unwrap_or_else(|_| {
                    fail(&format!("--slow-ms expects milliseconds, got `{v}`"))
                });
                fenec_http::metrics::set_slow(ms);
            }
            "--http-token" => http_cfg.token = Some(next(&mut i, "--http-token")),
            "--jwt-secret" => jwt_secret = Some(next(&mut i, "--jwt-secret")),
            "--jwt-keys" => jwt_keys = Some(next(&mut i, "--jwt-keys")),
            "--jwt-require-exp" => {
                demands.require_exp = match next(&mut i, "--jwt-require-exp").as_str() {
                    "on" => true,
                    "off" => false,
                    v => fail(&format!("--jwt-require-exp expects on or off, got `{v}`")),
                }
            }
            "--jwt-tenant-claim" => demands.tenant_claim = next(&mut i, "--jwt-tenant-claim"),
            "--jwt-unbound-tenants" => demands.unbound_tenants = true,
            "--jwt-max-age" => {
                let v = next(&mut i, "--jwt-max-age");
                let secs: u64 = v
                    .parse()
                    .unwrap_or_else(|_| fail(&format!("--jwt-max-age expects seconds, got `{v}`")));
                demands.max_age = (secs > 0).then_some(secs);
            }
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
            "--idempotency-ttl" => {
                let v = next(&mut i, "--idempotency-ttl");
                let s: u64 = v.parse().unwrap_or_else(|_| {
                    fail(&format!("--idempotency-ttl expects seconds, got `{v}`"))
                });
                http_cfg.idempotency_ttl = std::time::Duration::from_secs(s);
            }
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
            "--cdc" => cdc = true,
            "--replication-buffer" => {
                let v = next(&mut i, "--replication-buffer");
                let mib: usize = v.parse().unwrap_or_else(|_| {
                    fail(&format!("--replication-buffer expects MiB, got `{v}`"))
                });
                replication_buffer = mib << 20;
            }
            "--ping" => ping = true,
            "--no-mmap" => mmap = false,
            "--auto-compact" => {
                let v = next(&mut i, "--auto-compact");
                let policy = match v.as_str() {
                    "off" => None,
                    r => Some(
                        r.parse::<f64>()
                            .map_err(|_| Error::Query(String::new()))
                            .and_then(fenec_core::engine::CompactPolicy::at)
                            .unwrap_or_else(|_| {
                                fail(&format!(
                                    "--auto-compact expects a share of the file between 0 and 1, or off; got `{v}`"
                                ))
                            }),
                    ),
                };
                fenec_http::link::auto_compact(policy);
            }
            "--lease" => lease = true,
            "--warm" => {
                let v = next(&mut i, "--warm");
                warm = Some(match v.as_str() {
                    "off" => None,
                    v => Some(fenec_http::warm::Warm::parse(v).unwrap_or_else(|e| fail(&e))),
                });
            }
            "--insecure" => http_cfg.insecure = true,
            "--studio" => studio = true,
            "--studio-connect" => studio_connect = Some(next(&mut i, "--studio-connect")),
            "--follow" => follow_url = Some(next(&mut i, "--follow")),
            "--follow-table" => follow_table = Some(next(&mut i, "--follow-table")),
            "--follow-into" => follow_into = Some(next(&mut i, "--follow-into")),
            "--follow-slot" => follow_slot = Some(next(&mut i, "--follow-slot")),
            "--follow-publication" => {
                follow_publication = Some(next(&mut i, "--follow-publication"))
            }
            flag if flag.starts_with("--follow-") => {
                let v = next(&mut i, flag);
                let as_import = format!("--{}", &flag["--follow-".len()..]);
                match fenec_import::args::option(&mut follow_opts, &as_import, &v) {
                    Some(Ok(())) => follow_named = true,
                    Some(Err(e)) => fail(&e.replace(&as_import, flag)),
                    None => fail(&format!("unknown option: {flag}\n\n{USAGE}")),
                }
            }
            "--help" | "-h" => {
                fenec_http::log!("fenec-server {}\n\n{USAGE}", fenec_core::VERSION);
                return;
            }
            other => fail(&format!("unknown option: {other}\n\n{USAGE}")),
        }
        i += 1;
    }

    let access = |policy: &str| {
        match (&jwt_secret, &jwt_keys) {
            (Some(_), Some(_)) => {
                fail("--jwt-secret or --jwt-keys: the keys file holds the secret too")
            }
            (Some(secret), None) => fenec_http::access::Access::new(secret.as_bytes(), policy),
            (None, Some(path)) => {
                fenec_http::access::Access::from_jwks(std::path::Path::new(path), policy)
            }
            (None, None) => unreachable!(),
        }
        .map(|a| a.demanding(demands.clone()))
    };
    let keyed = jwt_secret.is_some() || jwt_keys.is_some();
    if let Some(claims) = mint {
        if !keyed {
            fail("--mint-token signs with --jwt-secret or --jwt-keys");
        }
        let access = access("").unwrap_or_else(|e| fail(&e));
        println!("{}", access.mint(&claims).unwrap_or_else(|e| fail(&e)));
        return;
    }
    match (keyed, policy) {
        (true, Some(policy)) => {
            let access = access(&policy).unwrap_or_else(|e| fail(&e));
            http_cfg.access = Some(Arc::new(access));
        }
        (true, None) => {
            fail("--jwt-secret and --jwt-keys need --policy: without rules a token reads nothing")
        }
        (false, Some(_)) => fail(
            "--policy needs --jwt-secret or --jwt-keys: the rules are for the tokens they verify",
        ),
        (false, None) => {}
    }

    match (studio, studio_connect.as_deref()) {
        (true, connect) => {
            let s = fenec_http::studio::Studio::new(fenec_http::studio::ASSETS, connect)
                .unwrap_or_else(|e| fail(&e));
            http_cfg.studio = Some(Arc::new(s));
        }
        (false, Some(_)) => fail("--studio-connect is for the studio: add --studio"),
        (false, None) => {}
    }

    if ping {
        std::process::exit(health_check(&http_cfg.addr));
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
    if replicating && sync == SyncPolicy::Off {
        fail(
            "--sync off puts nothing on disk before shutdown, and a replica is sent only \
             what is on the primary's disk: use --sync always or --sync <ms>",
        );
    }
    if cdc && file.is_none() {
        fail("--cdc keeps a file's writes: give --file (a --dir node's tenants have theirs with --replication-token)");
    }
    if cdc && sync == SyncPolicy::Off {
        fail("--sync off puts nothing on disk before shutdown, and /_changes hands over only what is on disk: use --sync always or --sync <ms>");
    }
    if lease && dir.is_none() {
        fail("--lease is a tenant node's, whose router grants it: give --dir");
    }

    // `--follow`: the follower writes the file this server serves, the one
    // writer of the collection it mirrors.
    let mirror = match follow_url {
        None => {
            let named = follow_table.is_some()
                || follow_into.is_some()
                || follow_slot.is_some()
                || follow_publication.is_some()
                || follow_named;
            if named {
                fail("the --follow-* options belong to --follow <postgres://...>");
            }
            None
        }
        Some(url) => {
            if dir.is_some() {
                fail("--follow mirrors a table into one file: give --file, not --dir");
            }
            if file.is_none() {
                fail(
                    "--follow mirrors a table into a file, and confirms to PostgreSQL only \
                     what is on its disk: give --file",
                );
            }
            if replica_of.is_some() {
                fail("--follow writes the file, and a replica's writes come from its primary: pick one");
            }
            let url = fenec_import::pg::Url::parse(&url).unwrap_or_else(|e| fail(&e.to_string()));
            let table =
                follow_table.unwrap_or_else(|| fail("--follow needs --follow-table <name>"));
            follow_opts.into = follow_into.unwrap_or_else(|| table.clone());
            let named = fenec_import::follow::Follow::named_after(&follow_opts.into);
            let follow = fenec_import::follow::Follow {
                slot: follow_slot.unwrap_or(named.slot),
                publication: follow_publication.unwrap_or(named.publication),
            };
            Some(fenec_server::mirror::Mirror {
                url,
                table,
                opts: follow_opts,
                follow,
            })
        }
    };

    // `--sync always` must hold for every write, and so must `--max-memory`;
    // both are the HTTP endpoint's to apply, as it takes every write.
    http_cfg.sync_on_write = sync == SyncPolicy::Always;

    if let Some(dir) = dir {
        if promote {
            fail(
                "a tenant is promoted one at a time, by the router: \
                 POST /_admin/tenants/<tenant>/promote",
            );
        }
        if file.is_some() {
            fail(
                "--dir and --file are exclusive: one serves a file, the other a directory of them",
            );
        }
        if metrics.is_some() {
            fail("with --dir the HTTP listener serves /_metrics: --metrics is for a --file server");
        }
        let repl = replication_token.filter(|_| replicating).map(|token| {
            fenec_http::tenants::Replicated {
                token,
                buffer: replication_buffer,
                upstream: replica_of.clone(),
                sync_on_write: sync == SyncPolicy::Always,
            }
        });
        serve_dir(
            &dir,
            http_cfg,
            sync,
            checkpoint,
            idle_close,
            mmap,
            lease,
            repl,
            warm.flatten(),
        );
    }

    let mut feed = None;
    let mut db = match &file {
        Some(path) => {
            let opened = if replicating || cdc {
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
            // Syncing makes no sense for an in-memory database, nor does a
            // checkpoint: there is no file to write to, but the image would
            // still be built in memory.
            sync = SyncPolicy::Off;
            http_cfg.sync_on_write = false;
            checkpoint = false;
            Database::new()
        }
    };

    if let Some(m) = &mirror {
        if let Err(e) = db.install_plugin(&fenec_server::mirror::GuardPlugin(m.opts.into.clone())) {
            fenec_http::log!("could not load the plugin: {e}");
            std::process::exit(1);
        }
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
    // The rows past their time (`@ttl`) are deleted once a minute, a
    // replica's by its primary.
    fenec_http::sweep::watch(file.as_deref().unwrap_or("the database"), &shared);

    // The follower applies the primary's writes; this server's own feed
    // passes them on to replicas of its own.
    let follower = replica_of.as_ref().map(|url| {
        let f = Follower::new(
            url,
            replication_token.clone().unwrap_or_default(),
            Arc::clone(&shared),
            feed.clone(),
            sync == SyncPolicy::Always,
        )
        .unwrap_or_else(|e| fail(&e));
        f.start("fenec-replica".into())
            .unwrap_or_else(|e| fail(&format!("could not start the replica thread: {e}")));
        fenec_http::log!("following: {url}");
        f
    });
    let repl = match replication_token.filter(|_| replicating) {
        Some(token) => Some(Replication::new(Some(token), feed.clone(), follower)),
        // The feed for `/_changes` alone: no token, so no replica is fed.
        None if cdc => Some(Replication::new(None, feed.clone(), None)),
        None => None,
    };

    // Before the HTTP thread announces its listener: once the line is out a
    // supervisor may send SIGTERM, and the flag it sets waits for the
    // syncer below.
    durability::install_signal_handlers();

    if let Some(m) = mirror {
        let table = m.table.clone();
        fenec_server::mirror::start(m, Arc::clone(&shared))
            .unwrap_or_else(|e| fail(&format!("could not start the follower: {e}")));
        fenec_http::log!("following: {table}");
    }

    // `--metrics`: /_metrics alone on a listener of its own, readable with
    // the tokens the HTTP endpoint takes.
    if let Some(addr) = metrics {
        let mcfg = fenec_http::Config {
            addr,
            token: http_cfg.token.clone(),
            admin_token: http_cfg.admin_token.clone(),
            insecure: http_cfg.insecure,
            idle_timeout: http_cfg.idle_timeout,
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
    fenec_http::log!(
        "fenec-server {}: {}  [sync={}]",
        fenec_core::VERSION,
        file.as_deref().unwrap_or("in memory"),
        sync.describe()
    );
    std::thread::Builder::new()
        .name("fenec-http".into())
        .spawn(move || {
            if let Err(e) = http_server.serve_on(listener) {
                fenec_http::log!("HTTP server error: {e}");
                std::process::exit(1);
            }
        })
        .unwrap_or_else(|e| fail(&format!("could not start the HTTP thread: {e}")));

    // `--warm`: the derived indexes built beside the first queries, which
    // otherwise build each on its first read. Once the endpoint is up:
    // `Server::new` takes the write lock to attach its watcher, and started
    // before it, a build's read lock held the listener back by the 141 ms a
    // `@text` index of 100 000 products takes.
    if let Some(w) = &warm.unwrap_or_else(|| Some(fenec_http::warm::Warm::default())) {
        fenec_http::warm::start(file.as_deref().unwrap_or("the database"), &shared, w);
    }

    durability::run_syncer(shared, sync, checkpoint);
}

/// `--dir`: the HTTP listener over a directory of tenants, and on this
/// thread the syncer a single file gets from [`durability::run_syncer`] --
/// periodic sync, idle close, and on the shutdown signal a final sync and
/// checkpoint of every open tenant.
#[allow(clippy::too_many_arguments)]
fn serve_dir(
    dir: &str,
    http_cfg: fenec_http::Config,
    sync: SyncPolicy,
    checkpoint: bool,
    idle_close: Duration,
    mmap: bool,
    lease: bool,
    repl: Option<fenec_http::tenants::Replicated>,
    warm: Option<fenec_http::warm::Warm>,
) -> ! {
    let tenants = match Tenants::new(dir) {
        Ok(t) => t,
        Err(e) => fail(&format!("could not use {dir}: {e}")),
    };
    let mut tenants = tenants
        .with_change_capacity(http_cfg.change_capacity)
        .with_max_memory(http_cfg.max_memory)
        .with_checkpoint(checkpoint)
        .with_mmap(mmap)
        .with_warm(warm);
    let follows = repl.as_ref().and_then(|r| r.upstream.clone());
    if let Some(r) = repl {
        tenants = tenants.with_replication(r);
    }
    if lease {
        tenants = tenants.with_lease();
        fenec_http::log!("taking writes under the router's lease: none until it grants one");
    }
    let tenants = Arc::new(tenants);
    fenec_http::log!(
        "serving tenants from: {dir}  ({} on disk)",
        tenants.names().len()
    );
    // A replica's tenants have to be open to follow: nothing else touches
    // them there, and a follower that is not running is a replica falling
    // behind. Opening one starts its follower -- every tenant on a standby,
    // and elsewhere those following a node of their own.
    if let Some(url) = &follows {
        fenec_http::log!("following the tenants of: {url}");
    }
    for (name, Refused(status, msg)) in tenants.resume_following() {
        fenec_http::log!("tenant `{name}` did not open ({status}): {msg}");
    }

    let http_server = fenec_http::Server::with_tenants(Arc::clone(&tenants), http_cfg);
    let listener = match http_server.bind() {
        Ok(l) => l,
        Err(e) => fail(&format!("could not open the HTTP endpoint: {e}")),
    };
    // Before the thread that announces the listener: once the line is out,
    // SIGTERM must sync.
    durability::install_signal_handlers();
    std::thread::Builder::new()
        .name("fenec-http".into())
        .spawn(move || {
            if let Err(e) = http_server.serve_on(listener) {
                fenec_http::log!("HTTP server error: {e}");
                std::process::exit(1);
            }
        })
        .unwrap_or_else(|e| fail(&format!("could not start the HTTP thread: {e}")));

    let tick = sync.tick();
    loop {
        std::thread::sleep(tick);
        if durability::shutdown_requested() {
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
