//! `fenec-shard` -- one HTTP front for many `fenec-server --dir` nodes.
//!
//! ```text
//! fenec-server --dir /data/n1 --http 127.0.0.1:8081 --admin-token a1
//! fenec-server --dir /data/n2 --http 127.0.0.1:8082 --admin-token a2
//! fenec-shard --listen 127.0.0.1:8090 --directory shard.fenec
//! curl -X PUT localhost:8090/_shard/nodes/n1 -d '{"addr":"127.0.0.1:8081","token":"a1"}'
//! curl -X PUT localhost:8090/_shard/tenants/acme
//! curl localhost:8090/t/acme/
//! ```

use fenec_http::replication::{self, Follower, Replication};
use fenec_shard::directory::Directory;
use fenec_shard::{Config, Router};
use std::sync::{Arc, RwLock};
use std::time::Duration;

const USAGE: &str = "\
usage: fenec-shard [options]

  -l, --listen <address>    default 127.0.0.1:8090. Port 0 takes one the system
                            picks: the `listening on: http://<address>` line
                            on stderr names it
  -d, --directory <path>    the directory file: which node holds which tenant
                            default: shard.fenec
      --token <value>       require `Authorization: Bearer <value>` on /_shard/.
                            Data requests pass their own Authorization through
                            to the nodes, which check it against --http-token
      --insecure            allow a non-loopback address without --token
      --http-cors <origin>  `Access-Control-Allow-Origin` (e.g. * or
                            https://example.com), as fenec-server takes it:
                            on every answer the router gives, its own and
                            the nodes' it forwards, whose CORS headers it
                            replaces, and a browser's preflight answered
                            here. Without it, the router sends none of its
                            own and passes the nodes' through
      --studio              serve fenec studio, the admin pages, at /_studio/:
                            a tenant's collections and rows in a browser,
                            through this router, with the token pasted in.
                            Off by default; keep it on a private network
      --entry-token <value> the global entry's ENTRY_TOKEN
                            (integrations/cloudflare/entry): a request whose
                            Fenec-Client header carries the entry's mark for
                            its address, made within 60 s of this clock, is
                            that client's -- its refusal's wait and count, a
                            good token's clearing, the audit line, the trace
                            and the client named to a node or another
                            region's router -- where it would be Cloudflare's
                            address. A header seen can be replayed for that
                            minute, naming that one address. Without it, or
                            for a header that does not check, the
                            connection's address counts. Every router drops
                            the header from what it forwards. A token of its
                            own, since the entry's Worker holds it: refused
                            equal to --token, --replication-token or a node's
                            admin token the directory holds; the nodes' data
                            and replication tokens this router never sees,
                            and those are yours to keep apart. One at a time:
                            to change it, start each router with the new one,
                            then put it in the Worker (ENTRY_TOKEN); between
                            the two every request is served, and its client
                            counted by Cloudflare's address, as bad_mark in
                            fenec_router_entry_clients_total
      --audit <path>        append a JSON line to this file for each request
                            refused for its token and each /_shard/ request
                            that changes something
      --auth-delay <ms>     how long a request refused for its token waits,
                            doubled for each one from the same address within
                            a minute, 5 s at most; 0 turns it off. default: 100
      --max-connections <n> ceiling on concurrent connections  default: 1000
      --max-body <MiB>      request body ceiling  default: 64
      --upstream-timeout <s> connect/read bound towards a node  default: 60
      --fanout-timeout <s>  the longest a query across tenants (POST /_shard/query)
                            runs, every call to a node within it; a request
                            may ask for less (timeout_ms)  default: 30
      --fanout-max-rows <n> rows such a query gathers from its tenants at
                            most: past it the query is refused (413), never
                            answered with what fitted  default: 100000
      --fanout-max-bytes <MiB>  bytes of the tenants' answers it gathers at
                            most, refused past it likewise  default: 64
      --fanout-concurrency <n>  calls to the nodes it has out at once, the
                            tenants taken node by node in turn  default: 16
      --replicas            give each tenant created on a node in no pair a
                            replica on another node of its region -- one in
                            another zone first, then the one holding the
                            fewest -- so a node's tenants fail over across
                            the others rather than onto one idle standby.
                            The nodes need --replication-token, the same one
      --default-region <name>  the region a tenant created naming neither a
                            region nor a node is placed in. Without it, such
                            a tenant goes to the node with the fewest bytes
                            whatever its region, and lives in that node's
      --auto-failover <s>   lease each node the tenants it holds for this long,
                            renewed every third of it, and fail a node over on
                            its own once it has gone a tenth past it unrenewed.
                            The nodes need --lease: a node the router cannot
                            reach stops taking writes as its lease lapses, so
                            no tenant has two primaries. Off by default; 5 is a
                            start. A standby router leases nothing until it is
                            promoted, and then waits out a lease before it
                            fails anything over
      --otlp-endpoint <url> send OpenTelemetry traces, OTLP over HTTP as JSON,
                            to this collector: http://localhost:4318 (the
                            OpenTelemetry Collector, the Datadog Agent's OTLP
                            receiver). No TLS: a collector on this host or
                            network forwards over TLS. Also read from
                            OTEL_EXPORTER_OTLP_TRACES_ENDPOINT and
                            OTEL_EXPORTER_OTLP_ENDPOINT. Off by default
      --otlp-header <k=v>   a header on every post, such as an API key;
                            repeatable. Also OTEL_EXPORTER_OTLP_HEADERS
      --trace-sample <ratio>  the share of the requests that start a trace
                            here that are kept, 0 to 1  default: 1 with an
                            endpoint. A request whose traceparent was sampled
                            is traced whatever the ratio. The service is
                            named by OTEL_SERVICE_NAME  default: fenec-shard
      --replication-token <value>  serve the directory to standby routers at
                            /_replication, and present this to a primary
      --replica-of <url>    follow the primary router at http://host:port:
                            the directory arrives from it, /_shard/ changes
                            are refused here, and forwarding goes on as ever
      --promote             take writes on a standby's directory file: its
                            history forks here. A standby's file opens only
                            with --replica-of or this
      --replication-buffer <MiB>  directory writes kept for standbys that fall
                            behind  default: 8
      --region <name>       keep this region's own directory in --directory:
                            its nodes and tenants, leased and failed over by
                            this router with no word from another region, so
                            a region cut off from the rest goes on taking
                            writes. Needs --global and --replication-token;
                            the global router records it with PUT
                            /_shard/regions/<name> {\"routers\": [..]}. With
                            --replica-of, a standby of the region's router,
                            promoted as any standby router is
      --global <url>        the global router, http://host:port, repeatable or
                            comma-separated, the one keeping the global
                            directory first: a region's router follows its
                            directory from it -- where every other tenant
                            lives -- and sends it the changes it takes: a
                            create, a delete, a move across regions
      --global-directory <path>  the region's router's copy of the global
                            directory  default: the --directory file with
                            .global before .fenec
      --near <region>       the region this router stands in: a read its
                            client marks (Fenec-Read: replica) of a tenant
                            with a read replica there goes to it, every
                            other request home. A region's router (--region)
                            is near its region already
      --replica-timeout <ms>  how long a read replica has to answer such a
                            read, past the wait its Fenec-After asks for;
                            one that does not is let be for 5 s, its
                            region's reads going home  default: 2000

The token is also read from the FENEC_SHARD_TOKEN environment variable, and
the entry's from FENEC_SHARD_ENTRY_TOKEN.
";

fn fail(msg: &str) -> ! {
    fenec_http::log!("{msg}");
    std::process::exit(2);
}

/// Refuses `--entry-token` where a node `dir` holds has it for its admin
/// token, which reaches `/_admin/` there; a node added later is refused it
/// as it is added (`PUT /_shard/nodes/<n>`).
fn refuse_node_token(dir: &Directory, entry_token: Option<&str>, path: &str) {
    let Some(t) = entry_token else { return };
    if let Some((name, _)) = dir.nodes().iter().find(|(_, n)| n.token == t) {
        fail(&format!(
            "--entry-token is node `{name}`'s admin token in {path}: the entry's Worker holds \
             its token, and that one reaches /_admin/ on the node; give the entry one of its own"
        ));
    }
}

fn main() {
    let mut cfg = Config {
        token: std::env::var("FENEC_SHARD_TOKEN").ok(),
        // Set and empty is unset, as a deployment's template leaves it.
        entry_token: std::env::var("FENEC_SHARD_ENTRY_TOKEN")
            .ok()
            .filter(|t| !t.is_empty()),
        ..Config::default()
    };
    let mut path = "shard.fenec".to_string();
    let mut replication_token: Option<String> = None;
    let mut replica_of: Option<String> = None;
    let mut promote = false;
    let mut replication_buffer = 8 << 20;
    let mut global_path: Option<String> = None;
    let mut tracing = fenec_http::trace::Options::default();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    let next = |i: &mut usize, flag: &str| -> String {
        *i += 1;
        args.get(*i)
            .cloned()
            .unwrap_or_else(|| fail(&format!("{flag} expects a value")))
    };
    let number = |v: String, flag: &str| -> u64 {
        v.parse()
            .unwrap_or_else(|_| fail(&format!("{flag} expects a number, got `{v}`")))
    };
    while i < args.len() {
        match args[i].as_str() {
            "--listen" | "-l" => cfg.addr = next(&mut i, "--listen"),
            "--directory" | "-d" => path = next(&mut i, "--directory"),
            "--token" => cfg.token = Some(next(&mut i, "--token")),
            "--entry-token" => cfg.entry_token = Some(next(&mut i, "--entry-token")),
            "--insecure" => cfg.insecure = true,
            "--http-cors" => cfg.cors = Some(next(&mut i, "--http-cors")),
            "--studio" => {
                let s = fenec_http::studio::Studio::new(fenec_http::studio::ASSETS, None)
                    .unwrap_or_else(|e| fail(&e));
                cfg.studio = Some(Arc::new(s));
            }
            "--audit" => {
                let p = next(&mut i, "--audit");
                if let Err(e) = fenec_http::audit::open(std::path::Path::new(&p)) {
                    fail(&format!("could not open {p}: {e}"));
                }
            }
            "--auth-delay" => {
                fenec_http::audit::set_delay(number(next(&mut i, "--auth-delay"), "--auth-delay"))
            }
            "--replicas" => cfg.replicas = true,
            "--default-region" => {
                let r = next(&mut i, "--default-region");
                if fenec_http::tenants::check_name(&r).is_err() {
                    fail(&format!(
                        "--default-region `{r}`: a region is named [a-z0-9_-], 1 to {} characters",
                        fenec_http::tenants::MAX_NAME
                    ));
                }
                cfg.default_region = Some(r);
            }
            "--otlp-endpoint" => tracing.endpoint = Some(next(&mut i, "--otlp-endpoint")),
            "--otlp-header" => {
                let kv = next(&mut i, "--otlp-header");
                tracing.header(&kv).unwrap_or_else(|e| fail(&e));
            }
            "--trace-sample" => {
                let v = next(&mut i, "--trace-sample");
                tracing.sample(&v).unwrap_or_else(|e| fail(&e));
            }
            "--auto-failover" => {
                let secs = number(next(&mut i, "--auto-failover"), "--auto-failover");
                if secs == 0 {
                    fail("--auto-failover is the lease's length in seconds: give one");
                }
                cfg.auto_failover = Some(Duration::from_secs(secs));
            }
            "--max-connections" => {
                cfg.max_connections =
                    number(next(&mut i, "--max-connections"), "--max-connections") as usize
            }
            "--max-body" => {
                cfg.max_body = (number(next(&mut i, "--max-body"), "--max-body") as usize) << 20
            }
            "--upstream-timeout" => {
                let secs = number(next(&mut i, "--upstream-timeout"), "--upstream-timeout");
                if secs == 0 {
                    fail("--upstream-timeout cannot be zero");
                }
                cfg.upstream_timeout = Duration::from_secs(secs);
            }
            "--fanout-timeout" => {
                let secs = number(next(&mut i, "--fanout-timeout"), "--fanout-timeout");
                if secs == 0 {
                    fail("--fanout-timeout cannot be zero");
                }
                cfg.fanout.timeout = Duration::from_secs(secs);
            }
            "--fanout-max-rows" => {
                cfg.fanout.max_rows =
                    number(next(&mut i, "--fanout-max-rows"), "--fanout-max-rows") as usize
            }
            "--fanout-max-bytes" => {
                let mib = number(next(&mut i, "--fanout-max-bytes"), "--fanout-max-bytes");
                cfg.fanout.max_bytes = (mib as usize) << 20;
            }
            "--fanout-concurrency" => {
                let n = number(next(&mut i, "--fanout-concurrency"), "--fanout-concurrency");
                if n == 0 {
                    fail("--fanout-concurrency is how many calls go out at once: 1 at least");
                }
                cfg.fanout.concurrency = n as usize;
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
            "--region" => {
                let r = next(&mut i, "--region");
                if fenec_http::tenants::check_name(&r).is_err() {
                    fail(&format!(
                        "--region `{r}`: a region is named [a-z0-9_-], 1 to {} characters",
                        fenec_http::tenants::MAX_NAME
                    ));
                }
                cfg.region = Some(r);
            }
            "--global" => {
                for url in next(&mut i, "--global").split(',').map(str::trim) {
                    if let Err(e) = fenec_http::replication::Upstream::check_node(url) {
                        fail(&e);
                    }
                    cfg.global.push(url.to_string());
                }
            }
            "--global-directory" => global_path = Some(next(&mut i, "--global-directory")),
            "--near" => {
                let r = next(&mut i, "--near");
                if fenec_http::tenants::check_name(&r).is_err() {
                    fail(&format!(
                        "--near `{r}`: a region is named [a-z0-9_-], 1 to {} characters",
                        fenec_http::tenants::MAX_NAME
                    ));
                }
                cfg.near = Some(r);
            }
            "--replica-timeout" => {
                let ms = number(next(&mut i, "--replica-timeout"), "--replica-timeout");
                if ms == 0 {
                    fail("--replica-timeout cannot be zero");
                }
                cfg.replica_timeout = Duration::from_millis(ms);
            }
            "--replication-buffer" => {
                let mib = number(next(&mut i, "--replication-buffer"), "--replication-buffer");
                replication_buffer = (mib as usize) << 20;
            }
            "--help" | "-h" => {
                fenec_http::log!("fenec-shard {}\n\n{USAGE}", fenec_core::VERSION);
                return;
            }
            other => fail(&format!("unknown option: {other}\n\n{USAGE}")),
        }
        i += 1;
    }

    match tracing.settings("fenec-shard") {
        Err(e) => fail(&e),
        Ok(None) => {}
        Ok(Some(s)) => {
            let (url, sample) = (s.endpoint.url(), s.sample);
            if let Err(e) = fenec_http::trace::install(s) {
                fail(&format!("could not start the tracing thread: {e}"));
            }
            fenec_http::log!("tracing: OTLP/HTTP JSON to {url}, sampling {sample}");
        }
    }

    // An empty key would let anyone make the entry's mark, and a token
    // the entry's Worker holds that this router takes for something else
    // would reach more than the client's name: `--token` moves and deletes
    // every tenant, and the directory's replication stream carries every
    // node's admin token.
    match cfg.entry_token.as_deref() {
        Some("") => fail("--entry-token is empty: the entry's mark would be anyone's to make"),
        Some(t) if cfg.token.as_deref() == Some(t) => fail(
            "--entry-token is --token: the entry's Worker holds its token, and --token moves \
             and deletes every tenant; give the entry one of its own",
        ),
        Some(t) if replication_token.as_deref() == Some(t) => fail(
            "--entry-token is --replication-token: the entry's Worker holds its token, and \
             the directory's replication stream carries every node's admin token; give the \
             entry one of its own",
        ),
        _ => {}
    }
    let replicating = replication_token.as_deref().is_some_and(|t| !t.is_empty());
    if replica_of.is_some() && !replicating {
        fail("--replica-of needs --replication-token: the primary asks for it");
    }
    if replica_of.is_some() && promote {
        fail("--replica-of follows a primary and --promote stops following: pick one");
    }
    if cfg.region.is_some() && (cfg.global.is_empty() || !replicating) {
        fail(
            "--region needs --global, the global router, and --replication-token: a region's \
             router follows the global directory, where every other tenant lives",
        );
    }
    if cfg.region.is_none() && (!cfg.global.is_empty() || global_path.is_some()) {
        fail(
            "--global names the global router to a region's router (--region); a router of the \
             global directory follows its primary with --replica-of",
        );
    }

    // The directory goes through the feed a standby reads it from; without
    // a token it is an ordinary file.
    let (db, feed) = if replicating {
        match replication::open(&path, replication_buffer) {
            Ok((db, feed)) => (db, Some(feed)),
            Err(e) => fail(&format!("could not open the directory {path}: {e}")),
        }
    } else {
        match fenec_core::fs::open(&path) {
            Ok(db) => (db, None),
            Err(e) => fail(&format!("could not open the directory {path}: {e}")),
        }
    };
    let mut db = db;
    if let Err(e) = replication::settle_history(
        &mut db,
        &path,
        ("a standby's directory", "directory changes"),
        replicating,
        replica_of.is_some(),
        promote,
    ) {
        fail(&e);
    }
    let db = Arc::new(RwLock::new(db));
    let mut dir = match Directory::load(Arc::clone(&db)) {
        Ok(d) => d,
        Err(e) => fail(&format!("could not read the directory {path}: {e}")),
    };
    refuse_node_token(&dir, cfg.entry_token.as_deref(), &path);
    if cfg.region.is_none() {
        if let Err(e) = dir.settle_scope(None) {
            fail(&format!("{path}: {e}"));
        }
    }

    // A region's router's copy of the global directory, followed from the
    // global router: where every tenant outside the region lives.
    let global = cfg.region.as_ref().map(|_| {
        let gpath = global_path
            .clone()
            .unwrap_or_else(|| match path.strip_suffix(".fenec") {
                Some(stem) => format!("{stem}.global.fenec"),
                None => format!("{path}.global"),
            });
        let (mut gdb, gfeed) = match replication::open(&gpath, replication_buffer) {
            Ok(opened) => opened,
            Err(e) => fail(&format!(
                "could not open the global directory's copy {gpath}: {e}"
            )),
        };
        let lineage = gdb.history().lineage.clone();
        if let Err(e) = gdb.follow(lineage) {
            fail(&format!("could not settle {gpath}'s history: {e}"));
        }
        let gdb = Arc::new(RwLock::new(gdb));
        let gdir = match Directory::load(Arc::clone(&gdb)) {
            Ok(d) => d,
            Err(e) => fail(&format!(
                "could not read the global directory's copy {gpath}: {e}"
            )),
        };
        refuse_node_token(&gdir, cfg.entry_token.as_deref(), &gpath);
        let f = Follower::new(
            &cfg.global[0],
            replication_token.clone().unwrap_or_default(),
            gdb,
            Some(gfeed),
            true,
        )
        .unwrap_or_else(|e| fail(&e));
        f.start("fenec-global".into()).unwrap_or_else(|e| {
            fail(&format!(
                "could not start the global directory's thread: {e}"
            ))
        });
        fenec_http::log!("following the global directory: {}", cfg.global[0]);
        gdir
    });

    // The follower applies the primary's directory writes; this router's own
    // feed passes them on to standbys of its own.
    let follower = replica_of.as_ref().map(|url| {
        let f = Follower::new(
            url,
            replication_token.clone().unwrap_or_default(),
            Arc::clone(&db),
            feed.clone(),
            true,
        )
        .unwrap_or_else(|e| fail(&e));
        f.start("fenec-standby".into())
            .unwrap_or_else(|e| fail(&format!("could not start the standby thread: {e}")));
        fenec_http::log!("following: {url}");
        f
    });
    let repl = replication_token
        .filter(|_| replicating)
        .map(|token| Replication::new(Some(token), feed, follower));
    let router = match (global, repl) {
        (Some(global), repl) => Router::regional(dir, cfg, repl, global)
            .unwrap_or_else(|e| fail(&format!("{path}: {e}"))),
        (None, Some(repl)) => Router::replicated(dir, cfg, repl),
        (None, None) => Router::new(dir, cfg),
    };
    let listener = match router.bind() {
        Ok(l) => l,
        Err(e) => fail(&format!("could not listen: {e}")),
    };
    if let Err(e) = router.start_leasing() {
        fail(&format!("could not start the leasing thread: {e}"));
    }
    if let Err(e) = router.serve_on(listener) {
        fenec_http::log!("router error: {e}");
        std::process::exit(1);
    }
}
