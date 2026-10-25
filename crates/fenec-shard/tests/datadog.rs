//! `monitoring/datadog/`'s check and dashboard, and `deploy/alerts/`'s
//! rules, against what `/_metrics` really exposes. Neither Datadog nor
//! Prometheus is run here: what can drift is a name, and a dashboard asking
//! for a metric no server sends draws an empty chart with no error, as an
//! alert over one never fires. So every metric `conf.yaml` keeps must be in a
//! scrape, and every metric `dashboard.json` asks for must be one `conf.yaml`
//! keeps -- named as the Agent's OpenMetrics check names it -- and in a
//! scrape too; and every metric an alert rule reads, with every label value
//! it matches, must be in a scrape, and Grafana's rules Prometheus's.
//!
//! The scrapes are of every kind of process there is, in this one: a
//! primary with a collection, its replica once it has heard from it, a
//! tenant node whose tenants replicate, a router and a router leasing its
//! nodes. A file of its own, as the metrics' tests are: a scrape counts the
//! process's own requests.

use fenec_core::prelude::*;
use fenec_http::replication::{self, fresh_id, Follower, Replication};
use fenec_http::tenants::{Replicated, Tenants};
use fenec_shard::directory::{Directory, Node, State};
use fenec_shard::{Config, Router};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

const REPL: &str = "replication-secret";

/// A directory of its own for each call: both tests scrape at once, each
/// its own servers, and a name shared by the process's id alone had one
/// remove the other's files from under it.
fn scratch(tag: &str) -> PathBuf {
    static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("fenec-datadog-{tag}-{}-{n}", std::process::id()));
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

/// Every kind of process there is, each scraped: the families they declare
/// with their types, and the scrapes' text, every sample line.
fn scrape() -> (BTreeMap<String, String>, String) {
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

    // A tenant node, its tenants replicated as `--replication-token` has
    // them (an archiver's stream counted), and the router in front of it.
    let tenants = Arc::new(
        Tenants::new(scratch("node"))
            .unwrap()
            .with_replication(Replicated {
                token: REPL.into(),
                buffer: 1 << 20,
                upstream: None,
                sync_on_write: true,
            }),
    );
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
    dir.set_node(
        "n1",
        Node {
            addr,
            token,
            ..Node::default()
        },
    )
    .unwrap();
    dir.place("acme", "n1", State::Active).unwrap();
    // Giving tenants replicas of their own (`--replicas`), as the kit's
    // routers do: its scrape counts the tenants without a copy, acme.
    let router = Router::new(
        dir,
        Config {
            addr: "127.0.0.1:0".into(),
            replicas: true,
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
    // A router under --auto-failover, whose scrape carries its leases. Its
    // lease is long and its leasing thread never started: no grant is made,
    // and the families are declared all the same.
    let leasing = Router::new(
        Directory::in_memory(),
        Config {
            addr: "127.0.0.1:0".into(),
            auto_failover: Some(Duration::from_secs(600)),
            ..Config::default()
        },
    );
    let listener = leasing.bind().unwrap();
    let leasing_addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let _ = leasing.serve_on(listener);
    });

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
    let mut text = replica_scrape;
    for (addr, token) in [
        (&primary, None),
        (&node, Some("adm")),
        (&router_addr, None),
        (&leasing_addr, None),
    ] {
        let (status, scraped) = call(addr, "GET", "/_metrics", "", token);
        assert_eq!(status, 200, "{addr}: {scraped}");
        families(&scraped, &mut exposed);
        text.push_str(&scraped);
    }
    (exposed, text)
}

#[test]
fn the_datadog_check_and_dashboard_name_only_what_the_servers_expose() {
    let (exposed, _) = scrape();
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

/// The rules of an alerting file, each a name and its expression, both on
/// one line as the files keep them: `- alert:` and `expr:` in Prometheus's,
/// `title:` and its query's `expr:` in Grafana's.
fn rules(text: &str, name_key: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for l in text.lines() {
        let t = l.trim();
        let t = t.strip_prefix("- ").unwrap_or(t);
        if let Some(name) = t.strip_prefix(name_key) {
            out.push((name.trim().to_string(), String::new()));
        } else if let Some(expr) = t.strip_prefix("expr:") {
            let last = out.last_mut().expect("an expr before any rule");
            assert!(last.1.is_empty(), "rule {} has two exprs", last.0);
            last.1 = expr.trim().to_string();
        }
    }
    out
}

/// Each metric an expression reads, and the label values its selector
/// matches: `name{label="v"}`, and `label=~"a|b"` as `a` and `b` each --
/// but `job` and `instance`, which Prometheus puts on every series it
/// scrapes and no server sends ([`jobs`] holds the jobs).
fn selectors(expr: &str) -> Vec<(String, Vec<(String, String)>)> {
    let b = expr.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(at) = expr[i..].find("fenec_") {
        let start = i + at;
        let mut end = start;
        while end < b.len() && (b[end].is_ascii_alphanumeric() || b[end] == b'_') {
            end += 1;
        }
        let mut matchers = Vec::new();
        i = end;
        if b.get(end) == Some(&b'{') {
            let close = end + expr[end..].find('}').expect("a selector left open");
            for m in expr[end + 1..close].split(',').map(str::trim) {
                let (label, value) = m
                    .split_once("=~")
                    .or_else(|| m.split_once('='))
                    .unwrap_or_else(|| panic!("a matcher this reads no label of: {m}"));
                // A negative one says nothing a scrape could show.
                assert!(!label.ends_with('!'), "`{m}`: hold it here before using it");
                if matches!(label.trim(), "job" | "instance") {
                    continue;
                }
                for v in value.trim_matches('"').split('|') {
                    matchers.push((label.trim().to_string(), v.to_string()));
                }
            }
            i = close;
        }
        out.push((expr[start..end].to_string(), matchers));
    }
    out
}

/// The jobs an expression names (`job="a"`, `job=~"a|b"`).
fn jobs(expr: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (i, _) in expr.match_indices("job=") {
        let rest = expr[i + 4..].trim_start_matches('~');
        let Some(quoted) = rest.strip_prefix('"') else {
            continue;
        };
        let value = &quoted[..quoted.find('"').expect("a job's name left open")];
        out.extend(value.split('|').map(str::to_string));
    }
    out
}

/// Metrics a rule reads that no fenecdb process exports, and where each
/// comes from instead.
const FROM_ELSEWHERE: [(&str, &str); 1] = [(
    // A run of the bucket sync that succeeded, into node_exporter's
    // textfile collector.
    "fenec_archive_sync_last_success_timestamp_seconds",
    "../../deploy/bin/fenec-archive-sync",
)];

/// node_exporter's own metrics a rule reads, beside Prometheus's `up`:
/// deploy/test/units.py scrapes a real node_exporter for them.
const NODE_EXPORTER: [&str; 1] = ["node_textfile_scrape_error"];

#[test]
fn the_alert_rules_read_only_what_the_servers_expose() {
    let (exposed, text) = scrape();
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../deploy/alerts");
    let read_rules =
        |file: &str, key: &str| rules(&std::fs::read_to_string(root.join(file)).unwrap(), key);
    let prometheus = read_rules("fenecdb.rules.yml", "alert:");
    let grafana = read_rules("grafana.yml", "title:");
    assert!(prometheus.len() > 15, "{prometheus:?}");
    assert_eq!(
        grafana, prometheus,
        "grafana.yml's rules are fenecdb.rules.yml's, title for alert and expression for \
         expression, in its order"
    );

    // The scrape jobs the file's header declares: a rule naming any other
    // matches no series, and a target-down rule over it never fires.
    let file = std::fs::read_to_string(root.join("fenecdb.rules.yml")).unwrap();
    let declared: BTreeSet<&str> = file
        .lines()
        .filter_map(|l| l.split_once("job_name:"))
        .map(|(_, rest)| rest.split_whitespace().next().unwrap())
        .collect();
    assert!(declared.contains("fenecdb-node"), "{declared:?}");

    let mut read = BTreeSet::new();
    for (alert, expr) in &prometheus {
        let found = selectors(expr);
        // Prometheus's own `up` is what a process that answers nothing
        // leaves to read.
        assert!(
            !found.is_empty()
                || expr.contains("up{")
                || NODE_EXPORTER.iter().any(|m| expr.contains(m)),
            "{alert} reads neither a fenec_ metric, `up` nor node_exporter's: `{expr}`"
        );
        for job in jobs(expr) {
            assert!(
                declared.contains(job.as_str()),
                "{alert} names job `{job}`, which the scrape config does not: {declared:?}"
            );
        }
        for (name, matchers) in found {
            if let Some((_, source)) = FROM_ELSEWHERE.iter().find(|(m, _)| *m == name) {
                let unit = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(source);
                assert!(
                    std::fs::read_to_string(&unit).unwrap().contains(&name),
                    "{alert} reads {name}, which {source} no longer writes"
                );
                read.insert(name);
                continue;
            }
            assert!(
                exposed.contains_key(&name),
                "{alert} reads {name}, which no server exposes: {:?}",
                exposed.keys().collect::<Vec<_>>()
            );
            for (label, value) in matchers {
                let head = format!("{name}{{");
                let pair = format!("{label}=\"{value}\"");
                assert!(
                    text.lines()
                        .any(|l| l.starts_with(&head) && l.contains(&pair)),
                    "{alert} matches {name}{{{pair}}}, a sample no server sends"
                );
            }
            read.insert(name);
        }
    }
    // And they watch what an operator of regions acts on.
    for want in [
        "fenec_router_lease_age_seconds",
        "fenec_router_auto_failovers_total",
        "fenec_router_node_failed_over",
        "fenec_replica_behind",
        "fenec_router_same_zone_copies",
        "fenec_router_tenants_unreplicated",
        "fenec_router_read_replicas_given_up",
        "fenec_quota_refused_total",
        "fenec_router_entry_clients_total",
        "fenec_storage_failed",
        "fenec_refused_total",
        "fenec_router_refused_total",
        "fenec_router_requests_total",
        "fenec_statement_errors_total",
        "fenec_node_streams",
        "fenec_connections",
        "fenec_archive_sync_last_success_timestamp_seconds",
    ] {
        assert!(read.contains(want), "no alert reads {want}");
    }
}
