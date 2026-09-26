//! `GET /_metrics` on the router: the requests it answered, by route and by
//! status class, how long the nodes took over the ones it forwarded, which
//! nodes it could not reach, and its moves -- in Prometheus's text format,
//! beside the nodes' own.
//!
//! Counted as a node counts its statements (`fenec_http::metrics`): a shard
//! a thread, since every connection is a thread and forwarding is every
//! request's path. No label names a tenant: a label per tenant would be as
//! many series as there are customers, and their names are not the
//! router's to publish.

use fenec_http::metrics::{histogram, shard, Text, Timings, BUCKETS, SHARD_COUNT};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

/// Where a request went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// `/t/<tenant>/...`, forwarded to the tenant's node.
    Tenant = 0,
    /// `/_shard/...`, the directory's endpoints.
    Shard = 1,
    /// `/_replication`, a standby router following this one.
    Replication = 2,
    /// `/_metrics`.
    Metrics = 3,
    /// Anything else, answered 404.
    Other = 4,
}

const ROUTES: [&str; 5] = ["tenant", "shard", "replication", "metrics", "other"];
const CODES: [&str; 4] = ["2xx", "3xx", "4xx", "5xx"];

/// Upper bounds of a move's buckets, in microseconds: 10 ms, a tenant of a
/// few rows, to 1 000 s, one of gigabytes.
const MOVE_BUCKETS: [u64; 16] = [
    10_000,
    25_000,
    50_000,
    100_000,
    250_000,
    500_000,
    1_000_000,
    2_500_000,
    5_000_000,
    10_000_000,
    25_000_000,
    50_000_000,
    100_000_000,
    250_000_000,
    500_000_000,
    1_000_000_000,
];

#[allow(clippy::declare_interior_mutable_const)]
const ZERO: AtomicU64 = AtomicU64::new(0);
#[allow(clippy::declare_interior_mutable_const)]
const TIMINGS: Timings = Timings::new(&BUCKETS);

/// One thread's counters -- a few threads' past sixteen. Aligned so that no
/// two shards share a cache line.
#[repr(align(128))]
struct Shard {
    /// Requests answered, `[route][code]`.
    requests: [[AtomicU64; CODES.len()]; ROUTES.len()],
    /// A request's arrival to its answer, by route: to its head, for a
    /// stream, which lasts as long as its client stays.
    durations: [Timings; ROUTES.len()],
    /// A forwarded request's from the moment it was sent to the node to the
    /// node's whole answer, or its head for a stream.
    upstream: Timings,
}

#[allow(clippy::declare_interior_mutable_const)]
const ROW: [AtomicU64; CODES.len()] = [ZERO; CODES.len()];
#[allow(clippy::declare_interior_mutable_const)]
const SHARD: Shard = Shard {
    requests: [ROW; ROUTES.len()],
    durations: [TIMINGS; ROUTES.len()],
    upstream: TIMINGS,
};

static SHARDS: [Shard; SHARD_COUNT] = [SHARD; SHARD_COUNT];
static CONNECTIONS: AtomicI64 = AtomicI64::new(0);
/// Requests a node did not answer or broke off, by node: rare, so a lock
/// does, and a node's name is what an operator acts on.
static UNREACHABLE: Mutex<BTreeMap<String, u64>> = Mutex::new(BTreeMap::new());
/// Moves `[done, failed]`, and how long a finished one took.
static MOVES: [AtomicU64; 2] = [ZERO; 2];
/// Automatic failovers `[every tenant promoted, some not]`.
static FAILOVERS: [AtomicU64; 2] = [ZERO; 2];
static MOVE_TIMES: Timings = Timings::new(&MOVE_BUCKETS);

/// Counts a request answered `status` after `took`.
pub fn request(route: Route, status: u16, took: Duration) {
    counted(route, status);
    SHARDS[shard()].durations[route as usize].add(took);
}

/// Counts a request and does not time it: a standby's replication stream,
/// whose head the stream writes itself and which lasts as long as the
/// standby stays connected.
pub fn counted(route: Route, status: u16) {
    let code = match status {
        0..=299 => 0,
        300..=399 => 1,
        400..=499 => 2,
        _ => 3,
    };
    SHARDS[shard()].requests[route as usize][code].fetch_add(1, Ordering::Relaxed);
}

/// Counts the time a node took over a forwarded request.
pub fn upstream(took: Duration) {
    SHARDS[shard()].upstream.add(took);
}

/// Counts a forwarded request the node `node` did not answer, or broke off.
pub fn unreachable(node: &str) {
    let mut m = UNREACHABLE.lock().unwrap_or_else(|e| e.into_inner());
    match m.get_mut(node) {
        Some(n) => *n += 1,
        None => {
            m.insert(node.to_string(), 1);
        }
    }
}

/// Counts a move that began, and how long it took when it was done.
pub fn moved(done: bool, took: Duration) {
    MOVES[!done as usize].fetch_add(1, Ordering::Relaxed);
    if done {
        MOVE_TIMES.add(took);
    }
}

/// Counts a failover the router made on its own, and whether it promoted
/// every tenant of the node.
pub fn failed_over(done: bool) {
    FAILOVERS[!done as usize].fetch_add(1, Ordering::Relaxed);
}

/// A client connection, counted open for as long as this lives.
pub struct Connection(());

impl Connection {
    pub fn open() -> Connection {
        CONNECTIONS.fetch_add(1, Ordering::Relaxed);
        Connection(())
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        CONNECTIONS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// The counters' part of a scrape.
pub fn counters(out: &mut Text) {
    out.family(
        "fenec_router_requests_total",
        "counter",
        "Requests answered, by route and by status class.",
    );
    for (r, route) in ROUTES.iter().enumerate() {
        for (c, code) in CODES.iter().enumerate() {
            let n: u64 = SHARDS
                .iter()
                .map(|s| s.requests[r][c].load(Ordering::Relaxed))
                .sum();
            out.sample(
                "fenec_router_requests_total",
                &[("route", route), ("code", code)],
                n,
            );
        }
    }
    out.family(
        "fenec_router_request_duration_seconds",
        "histogram",
        "From a request's arrival to its answer, the node's time included; to its head for a stream.",
    );
    for (r, route) in ROUTES.iter().enumerate() {
        let shards: Vec<&Timings> = SHARDS.iter().map(|s| &s.durations[r]).collect();
        histogram(
            out,
            "fenec_router_request_duration_seconds",
            &[("route", route)],
            &shards,
        );
    }
    out.family(
        "fenec_router_upstream_duration_seconds",
        "histogram",
        "From a forwarded request's sending to the node's whole answer; to its head for a stream.",
    );
    let shards: Vec<&Timings> = SHARDS.iter().map(|s| &s.upstream).collect();
    histogram(out, "fenec_router_upstream_duration_seconds", &[], &shards);
    out.family(
        "fenec_router_upstream_errors_total",
        "counter",
        "Forwarded requests a node did not answer or broke off, each answered 502, by node.",
    );
    for (node, n) in UNREACHABLE.lock().unwrap_or_else(|e| e.into_inner()).iter() {
        out.sample("fenec_router_upstream_errors_total", &[("node", node)], n);
    }
    out.family(
        "fenec_router_moves_total",
        "counter",
        "Tenant moves that began, by whether they were done or failed and thawed.",
    );
    for (i, outcome) in ["done", "failed"].iter().enumerate() {
        out.sample(
            "fenec_router_moves_total",
            &[("outcome", outcome)],
            MOVES[i].load(Ordering::Relaxed),
        );
    }
    out.family(
        "fenec_router_auto_failovers_total",
        "counter",
        "Nodes failed over on their own once their lease lapsed, by whether every tenant was promoted.",
    );
    for (i, outcome) in ["done", "failed"].iter().enumerate() {
        out.sample(
            "fenec_router_auto_failovers_total",
            &[("outcome", outcome)],
            FAILOVERS[i].load(Ordering::Relaxed),
        );
    }
    out.family(
        "fenec_router_move_duration_seconds",
        "histogram",
        "How long a move that was done took, the tenant frozen for all of it.",
    );
    histogram(
        out,
        "fenec_router_move_duration_seconds",
        &[],
        &[&MOVE_TIMES],
    );
    out.family(
        "fenec_router_connections",
        "gauge",
        "Client connections open now.",
    );
    out.sample(
        "fenec_router_connections",
        &[],
        CONNECTIONS.load(Ordering::Relaxed),
    );
}
