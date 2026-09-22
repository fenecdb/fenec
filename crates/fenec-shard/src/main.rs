//! `fenec-shard` -- one HTTP front for many `fenec-pg --dir` nodes.
//!
//! ```text
//! fenec-pg --dir /data/n1 --http 127.0.0.1:8081 --admin-token a1
//! fenec-pg --dir /data/n2 --http 127.0.0.1:8082 --admin-token a2
//! fenec-shard --listen 127.0.0.1:8090 --directory shard.fenec
//! curl -X PUT localhost:8090/_shard/nodes/n1 -d '{"addr":"127.0.0.1:8081","token":"a1"}'
//! curl -X PUT localhost:8090/_shard/tenants/acme
//! curl localhost:8090/t/acme/
//! ```

use fenec_core::prelude::*;
use fenec_http::replication::{self, fresh_id, Follower, Replication};
use fenec_shard::directory::Directory;
use fenec_shard::{Config, Router};
use std::sync::{Arc, RwLock};
use std::time::Duration;

const USAGE: &str = "\
usage: fenec-shard [options]

  -l, --listen <address>    default 127.0.0.1:8090
  -d, --directory <path>    the directory file: which node holds which tenant
                            default: shard.fenec
      --token <value>       require `Authorization: Bearer <value>` on /_shard/.
                            Data requests pass their own Authorization through
                            to the nodes, which check it against --http-token
      --insecure            allow a non-loopback address without --token
      --max-connections <n> ceiling on concurrent connections  default: 1000
      --max-body <MiB>      request body ceiling  default: 64
      --upstream-timeout <s> connect/read bound towards a node  default: 60
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

The token is also read from the FENEC_SHARD_TOKEN environment variable.
";

fn fail(msg: &str) -> ! {
    fenec_http::log!("{msg}");
    std::process::exit(2);
}

/// Which history the directory file belongs to, as `fenec-pg` settles a
/// database's: a standby's file follows, `--promote` forks it, and a primary
/// that has none starts one.
fn settle_history(
    db: &mut Database,
    path: &str,
    replicating: bool,
    follows: bool,
    promote: bool,
) -> std::result::Result<(), String> {
    let following = db.history().following;
    let result = if follows {
        let lineage = db.history().lineage.clone();
        db.follow(lineage)
    } else if promote {
        if !following {
            fenec_http::log!("--promote: {path} is not a standby's file; it opens as it is");
            return Ok(());
        }
        let id = fresh_id();
        let r = db.fork(id).and_then(|_| db.sync());
        if r.is_ok() {
            fenec_http::log!(
                "promoted: {path} takes directory changes from {} on, history {id:016x}",
                db.change_seq()
            );
        }
        r
    } else if following {
        return Err(format!(
            "{path} is a standby's directory. Start it with --replica-of <primary> to go on \
             following, or with --promote to take changes -- its history forks there"
        ));
    } else if replicating && db.history().lineage.is_empty() {
        db.fork(fresh_id()).and_then(|_| db.sync())
    } else {
        Ok(())
    };
    result.map_err(|e| format!("could not settle {path}'s history: {e}"))
}

fn main() {
    let mut cfg = Config {
        token: std::env::var("FENEC_SHARD_TOKEN").ok(),
        ..Config::default()
    };
    let mut path = "shard.fenec".to_string();
    let mut replication_token: Option<String> = None;
    let mut replica_of: Option<String> = None;
    let mut promote = false;
    let mut replication_buffer = 8 << 20;
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
            "--insecure" => cfg.insecure = true,
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
            "--replication-token" => replication_token = Some(next(&mut i, "--replication-token")),
            "--replica-of" => replica_of = Some(next(&mut i, "--replica-of")),
            "--promote" => promote = true,
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

    let replicating = replication_token.as_deref().is_some_and(|t| !t.is_empty());
    if replica_of.is_some() && !replicating {
        fail("--replica-of needs --replication-token: the primary asks for it");
    }
    if replica_of.is_some() && promote {
        fail("--replica-of follows a primary and --promote stops following: pick one");
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
    if let Err(e) = settle_history(&mut db, &path, replicating, replica_of.is_some(), promote) {
        fail(&e);
    }
    let db = Arc::new(RwLock::new(db));
    let dir = match Directory::load(Arc::clone(&db)) {
        Ok(d) => d,
        Err(e) => fail(&format!("could not read the directory {path}: {e}")),
    };

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
        let run = Arc::clone(&f);
        std::thread::Builder::new()
            .name("fenec-standby".into())
            .spawn(move || run.run())
            .unwrap_or_else(|e| fail(&format!("could not start the standby thread: {e}")));
        fenec_http::log!("following: {url}");
        f
    });
    let router = match replication_token.filter(|_| replicating) {
        Some(token) => Router::replicated(dir, cfg, Replication::new(token, feed, follower)),
        None => Router::new(dir, cfg),
    };
    let listener = match router.bind() {
        Ok(l) => l,
        Err(e) => fail(&format!("could not listen: {e}")),
    };
    if let Err(e) = router.serve_on(listener) {
        fenec_http::log!("router error: {e}");
        std::process::exit(1);
    }
}
