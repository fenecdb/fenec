//! `monitoring/datadog/`'s check and dashboard against what `/_metrics`
//! really exposes. Datadog is not run here: what can drift is a name, and a
//! dashboard asking for a metric no server sends draws an empty chart with
//! no error. So every metric `conf.yaml` keeps must be in a scrape, and every
//! metric `dashboard.json` asks for must be one `conf.yaml` keeps -- named
//! as the Agent's OpenMetrics check names it -- and in a scrape too.
//!
//! The scrapes are of every kind of process there is, in this one: a
//! primary with a collection, its replica once it has heard from it, a
//! tenant node and the router. A file of its own, as the metrics' tests
//! are: a scrape counts the process's own requests.

use fenec_core::prelude::*;
use fenec_http::replication::{self, fresh_id, Follower, Replication};
use fenec_http::tenants::Tenants;
use fenec_shard::directory::{Directory, Node, State};
use fenec_shard::{Config, Router};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

const REPL: &str = "replication-secret";

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fenec-datadog-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn serve(server: fenec_http::Server) -> String {
    let listener = server.bind().unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    addr
}

fn cfg() -> fenec_http::Config {
    fenec_http::Config {
        addr: "127.0.0.1:0".into(),
        ..fenec_http::Config::default()
    }
}

fn call(addr: &str, method: &str, target: &str, body: &str, auth: Option<&str>) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let auth = auth.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    write!(
        s,
        "{method} {target} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n{auth}\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    let status = out[9..12].parse().unwrap();
    let body = out
        .split_once("\r\n\r\n")
        .map_or("", |(_, b)| b)
        .to_string();
    (status, body)
}

/// The families a scrape declares, by name, with their type.
fn families(text: &str, into: &mut BTreeMap<String, String>) {
    for l in text.lines() {
        if let Some(rest) = l.strip_prefix("# TYPE ") {
            let (name, kind) = rest.split_once(' ').unwrap();
            into.insert(name.to_string(), kind.to_string());
        }
    }
}

/// What the Agent's OpenMetrics check calls a family, with `namespace:
/// fenecdb` and `raw_metric_prefix: fenec_`: its name below the prefix
/// (a counter's without `_total`), and the metrics it is sent as.
fn datadog_names(family: &str, kind: &str) -> (String, Vec<String>) {
    let raw = family.strip_prefix("fenec_").unwrap();
    match kind {
        "counter" => {
            let raw = raw.strip_suffix("_total").unwrap_or(raw);
            (raw.to_string(), vec![format!("fenecdb.{raw}.count")])
        }
        "histogram" => (
            raw.to_string(),
            ["bucket", "sum", "count"]
                .iter()
                .map(|s| format!("fenecdb.{raw}.{s}"))
                .collect(),
        ),
        _ => (raw.to_string(), vec![format!("fenecdb.{raw}")]),
    }
}

/// The entries of `conf.yaml`'s `metrics:` list: a name, or `^prefix.*$`.
fn kept(conf: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut inside = false;
    for l in conf.lines() {
        let t = l.trim();
        if t == "metrics:" {
            inside = true;
            continue;
        }
        if !inside || t.starts_with('#') || t.is_empty() {
            continue;
        }
        match t.strip_prefix("- ") {
            Some(entry) => out.push(entry.to_string()),
            None => break,
        }
    }
    out
}

fn keeps(entry: &str, raw: &str) -> bool {
    match entry.strip_prefix('^').and_then(|e| e.strip_suffix(".*$")) {
        Some(prefix) => raw.starts_with(prefix),
        None => entry == raw,
    }
}

/// Every `fenecdb.<name>` the dashboard's strings hold.
fn asked(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Text(t) => {
            for (i, _) in t.match_indices("fenecdb.") {
                let name: String = t[i..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '.')
                    .collect();
                out.push(name);
            }
        }
        Value::List(items) => items.iter().for_each(|i| asked(i, out)),
        Value::Object(members) => members.iter().for_each(|(_, m)| asked(m, out)),
        _ => {}
    }
}

#[test]
fn the_datadog_check_and_dashboard_name_only_what_the_servers_expose() {
    // A primary with a collection, as `--replication-token` starts one.
    let d = scratch("primary");
    let (mut db, feed) = replication::open(d.join("p.fenec").to_str().unwrap(), 1 << 20).unwrap();
    db.fork(fresh_id()).unwrap();
    for sql in [
        "create collection notes (title text, e vector<2> @hnsw(cosine))",
        "put notes {title: \"a\", e: [1.0, 0.0]}",
    ] {
        db.execute(&fenec_ql::parse_one(sql).unwrap()).unwrap();
    }
    db.sync().unwrap();
    let db = Arc::new(RwLock::new(db));
    let primary = serve(
        fenec_http::Server::new(Arc::clone(&db), cfg()).with_replication(Replication::new(
            Some(REPL.into()),
            Some(feed),
            None,
        )),
    );

    // Its replica, as `--replica-of` starts one.
    let d = scratch("replica");
    let (mut rdb, rfeed) = replication::open(d.join("r.fenec").to_str().unwrap(), 1 << 20).unwrap();
    let lineage = rdb.history().lineage.clone();
    rdb.follow(lineage).unwrap();
    let rdb = Arc::new(RwLock::new(rdb));
    let follower = Follower::new(
        &format!("http://{primary}"),
        REPL.into(),
        Arc::clone(&rdb),
        Some(Arc::clone(&rfeed)),
        true,
    )
    .unwrap();
    let f = Arc::clone(&follower);
    std::thread::spawn(move || f.run());
    let replica = serve(
        fenec_http::Server::new(rdb, cfg()).with_replication(Replication::new(
            Some(REPL.into()),
            Some(rfeed),
            Some(follower),
        )),
    );

    // A tenant node and the router in front of it.
    let tenants = Arc::new(Tenants::new(scratch("node")).unwrap());
    let node = serve(fenec_http::Server::with_tenants(
        tenants,
        fenec_http::Config {
            admin_token: Some("adm".into()),
            ..cfg()
        },
    ));
    let mut dir = Directory::in_memory();
    let token = "adm".into();
    let addr = node.clone();
    dir.set_node("n1", Node { addr, token }).unwrap();
    dir.place("acme", "n1", State::Active).unwrap();
    let router = Router::new(
        dir,
        Config {
            addr: "127.0.0.1:0".into(),
            ..Config::default()
        },
    );
    let listener = router.bind().unwrap();
    let router_addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let _ = router.serve_on(listener);
    });
    // A request it forwards, so its histograms have a sample.
    call(&router_addr, "GET", "/t/acme/", "", None);

    // The replica's gauges are there once it has heard from its primary.
    let deadline = Instant::now() + Duration::from_secs(20);
    let replica_scrape = loop {
        let (status, text) = call(&replica, "GET", "/_metrics", "", None);
        assert_eq!(status, 200, "{text}");
        if text.contains("\nfenec_replica_last_contact_seconds ") {
            break text;
        }
        assert!(Instant::now() < deadline, "the replica never connected");
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut exposed = BTreeMap::new();
    families(&replica_scrape, &mut exposed);
    for (addr, token) in [(&primary, None), (&node, Some("adm")), (&router_addr, None)] {
        let (status, text) = call(addr, "GET", "/_metrics", "", token);
        assert_eq!(status, 200, "{addr}: {text}");
        families(&text, &mut exposed);
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../monitoring/datadog");
    let conf = std::fs::read_to_string(root.join("conf.yaml")).unwrap();
    let entries = kept(&conf);
    assert!(entries.len() > 10, "{entries:?}");
    let mut arrives = BTreeMap::new();
    for (family, kind) in &exposed {
        let (raw, names) = datadog_names(family, kind);
        if entries.iter().any(|e| keeps(e, &raw)) {
            for n in names {
                arrives.insert(n, family.clone());
            }
        }
    }
    for e in &entries {
        assert!(
            exposed
                .iter()
                .any(|(f, k)| keeps(e, &datadog_names(f, k).0)),
            "conf.yaml keeps `{e}`, which no server exposes: {:?}",
            exposed.keys().collect::<Vec<_>>()
        );
    }

    let text = std::fs::read_to_string(root.join("dashboard.json")).unwrap();
    let dashboard = fenec_core::json::parse_json(&text).expect("dashboard.json is not JSON");
    let mut names = Vec::new();
    asked(&dashboard, &mut names);
    assert!(names.len() > 30, "{names:?}");
    let missing: Vec<&String> = names.iter().filter(|n| !arrives.contains_key(*n)).collect();
    assert!(
        missing.is_empty(),
        "the dashboard asks for metrics no server sends through conf.yaml: {missing:?}\n\
         what arrives: {:?}",
        arrives.keys().collect::<Vec<_>>()
    );
    // And it charts the things an operator acts on.
    for want in [
        "fenecdb.statements.count",
        "fenecdb.statement_duration_seconds.bucket",
        "fenecdb.change_sequence",
        "fenecdb.auto_compactions.count",
        "fenecdb.replica_behind",
        "fenecdb.tenants",
        "fenecdb.refused.count",
        "fenecdb.router_requests.count",
    ] {
        assert!(
            names.iter().any(|n| n == want),
            "no {want} in the dashboard"
        );
    }
}
