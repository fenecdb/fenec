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

use fenec_shard::directory::Directory;
use fenec_shard::{Config, Router};
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

The token is also read from the FENEC_SHARD_TOKEN environment variable.
";

fn fail(msg: &str) -> ! {
    fenec_http::log!("{msg}");
    std::process::exit(2);
}

fn main() {
    let mut cfg = Config {
        token: std::env::var("FENEC_SHARD_TOKEN").ok(),
        ..Config::default()
    };
    let mut path = "shard.fenec".to_string();
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
            "--help" | "-h" => {
                fenec_http::log!("fenec-shard {}\n\n{USAGE}", fenec_core::VERSION);
                return;
            }
            other => fail(&format!("unknown option: {other}\n\n{USAGE}")),
        }
        i += 1;
    }

    let dir = match Directory::open(&path) {
        Ok(d) => d,
        Err(e) => fail(&format!("could not open the directory {path}: {e}")),
    };
    let router = Router::new(dir, cfg);
    let listener = match router.bind() {
        Ok(l) => l,
        Err(e) => fail(&format!("could not listen: {e}")),
    };
    if let Err(e) = router.serve_on(listener) {
        fenec_http::log!("router error: {e}");
        std::process::exit(1);
    }
}
