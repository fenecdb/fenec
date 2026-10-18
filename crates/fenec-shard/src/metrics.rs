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
/// The status classes, and `gone`: a request whose client reset its
/// connection while the router waited on its node, answered nothing and
/// counted nowhere else. A reset alone: a client gone with its end -- most
/// timeouts and aborts -- reads as one that half-closed and reads on, so
/// its request is passed on to the node as one, answered as the node
/// answers it and counted by that answer's status
/// (`upstream::answered_or_gone`).
const CODES: [&str; 5] = ["2xx", "3xx", "4xx", "5xx", "gone"];

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
    /// What the global entry's name for a request's client came to, by
    /// [`EntryClient`]: behind the entry every request carries one, so it
    /// is counted in the thread's shard as a request is.
    entry: [AtomicU64; ENTRY_CLIENT.len()],
}

#[allow(clippy::declare_interior_mutable_const)]
const ROW: [AtomicU64; CODES.len()] = [ZERO; CODES.len()];
#[allow(clippy::declare_interior_mutable_const)]
const SHARD: Shard = Shard {
    requests: [ROW; ROUTES.len()],
    durations: [TIMINGS; ROUTES.len()],
    upstream: TIMINGS,
    entry: [ZERO; ENTRY_CLIENT.len()],
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
/// Queries across tenants (`POST /_shard/query`), by [`Fanned`].
static FANOUTS: [AtomicU64; 4] = [ZERO; 4];
static FANOUT_TIMES: Timings = Timings::new(&BUCKETS);
/// The tenants those queries asked: `[answered, failed]`.
static FANOUT_TENANTS: [AtomicU64; 2] = [ZERO; 2];

/// Reads marked for a replica (`Fenec-Read: replica`), by [`ReplicaRead`].
static REPLICA_READS: [AtomicU64; 3] = [ZERO; 3];
static GIVEN_UP: AtomicI64 = AtomicI64::new(0);

/// A read replica given up on (`1`), or let go of (`-1`): see
/// `fenec_router_read_replicas_given_up`.
pub fn given_up(by: i64) {
    GIVEN_UP.fetch_add(by, Ordering::Relaxed);
}

/// Where a read its client marked for a replica was answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicaRead {
    /// By the tenant's read replica in the router's region.
    Replica = 0,
    /// At home, the replica not reached, going down or behind the write
    /// the read waited for.
    Fallback = 1,
    /// At home, the tenant having no replica in the router's region -- or
    /// its home being that region.
    Home = 2,
}

const REPLICA_READ: [&str; 3] = ["replica", "fallback", "home"];

/// What a router with `--entry-token` made of a request's `Fenec-Client`
/// header, the global entry's name for its client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryClient {
    /// Its mark checked and its time was within a minute: the client named
    /// is the request's.
    Named = 0,
    /// Its mark is not the entry's -- another token, or a client sending
    /// the header itself -- and the connection's address counts.
    BadMark = 1,
    /// Its mark is the entry's, made more than a minute from this clock --
    /// replayed, or a clock off -- and the connection's address counts.
    Stale = 2,
    /// A tenant's request (`/t/`) no other router relayed carried none: a
    /// client reaching the router itself, or an entry whose `ENTRY_TOKEN`
    /// is not set, which names no client at all.
    Absent = 3,
}

const ENTRY_CLIENT: [&str; 4] = ["named", "bad_mark", "stale", "absent"];

/// Counts what a request's `Fenec-Client` header came to.
pub fn entry_client(how: EntryClient) {
    SHARDS[shard()].entry[how as usize].fetch_add(1, Ordering::Relaxed);
}

/// Counts a read marked for a replica by where it was answered.
pub fn replica_read(how: ReplicaRead) {
    REPLICA_READS[how as usize].fetch_add(1, Ordering::Relaxed);
}

/// How a query across tenants ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fanned {
    /// Every tenant answered, and the rows were merged.
    Done = 0,
    /// Some did not, and the caller asked for the others' rows, which it
    /// was given with the failures named.
    Partial = 1,
    /// Some did not, and the query failed naming them.
    Failed = 2,
    /// Refused as a whole: the request did not read, a bound was passed,
    /// or a merge could not be made.
    Refused = 3,
}

const FANNED: [&str; 4] = ["done", "partial", "failed", "refused"];

/// Counts a query across tenants: how it ended, how long it took, and how
/// many tenants answered and failed.
pub fn fanned(how: Fanned, took: Duration, answered: u64, failed: u64) {
    FANOUTS[how as usize].fetch_add(1, Ordering::Relaxed);
    FANOUT_TIMES.add(took);
    FANOUT_TENANTS[0].fetch_add(answered, Ordering::Relaxed);
    FANOUT_TENANTS[1].fetch_add(failed, Ordering::Relaxed);
}

/// Times a request answered: from its arrival to its answer written, or a
/// stream's head. Called once the answer is out, after [`counted`].
pub fn timed(route: Route, took: Duration) {
    SHARDS[shard()].durations[route as usize].add(took);
}

/// Counts and times a request whose client reset its connection before
/// its answer: the `gone` class, no status having been written.
pub fn gone(route: Route, took: Duration) {
    SHARDS[shard()].requests[route as usize][4].fetch_add(1, Ordering::Relaxed);
    SHARDS[shard()].durations[route as usize].add(took);
}

/// Counts a request answered `status`. Called before the answer is
/// written, as a node records its statements
/// (`fenec_http::metrics::record`), so a client holding its answer finds
/// it counted; [`timed`] once it is written. Counted after the write, as
/// it was, a scrape sent once the answer came could run before the thread
/// that wrote it had counted it: the entry's tests read eu's router a
/// request short after a relayed answer, and a request over once it landed
/// (CI, `entry.test.js`). The count alone goes first: count, clock and
/// histogram moved before the write took the router's added p50 0.1 us up
/// (`make shard-bench ARGS=latency`, ABBA). A standby's replication
/// stream, whose head the stream writes itself and which lasts as long as
/// the standby stays connected, is counted as it ends and not timed.
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
        "fenec_router_refused_total",
        "counter",
        "Requests refused for their token (401), the router's own and its nodes' it passed on, each also in the audit log.",
    );
    out.sample(
        "fenec_router_refused_total",
        &[],
        fenec_http::audit::refused(),
    );
    out.family(
        "fenec_router_requests_total",
        "counter",
        "Requests answered, by route and by status class; `gone`, a request whose client reset its connection before its answer (a client gone with its end is answered, and counted by the answer's status).",
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
        "fenec_router_fanouts_total",
        "counter",
        "Queries across tenants (POST /_shard/query), by how they ended: done, partial (some tenants failed and the caller asked for the rest), failed (naming the tenants), refused (as a whole).",
    );
    for (i, outcome) in FANNED.iter().enumerate() {
        out.sample(
            "fenec_router_fanouts_total",
            &[("outcome", outcome)],
            FANOUTS[i].load(Ordering::Relaxed),
        );
    }
    out.family(
        "fenec_router_fanout_duration_seconds",
        "histogram",
        "How long a query across tenants took, from its arrival to the merged answer.",
    );
    histogram(
        out,
        "fenec_router_fanout_duration_seconds",
        &[],
        &[&FANOUT_TIMES],
    );
    out.family(
        "fenec_router_fanout_tenants_total",
        "counter",
        "Tenants the queries across tenants asked, by whether each answered or failed.",
    );
    for (i, outcome) in ["answered", "failed"].iter().enumerate() {
        out.sample(
            "fenec_router_fanout_tenants_total",
            &[("outcome", outcome)],
            FANOUT_TENANTS[i].load(Ordering::Relaxed),
        );
    }
    out.family(
        "fenec_router_replica_reads_total",
        "counter",
        "Reads their client marked for a replica (Fenec-Read: replica), by where they were answered: replica (the tenant's read replica in the router's region), fallback (home, the replica not reached or behind the write the read waited for), home (no replica in the router's region).",
    );
    for (i, outcome) in REPLICA_READ.iter().enumerate() {
        out.sample(
            "fenec_router_replica_reads_total",
            &[("outcome", outcome)],
            REPLICA_READS[i].load(Ordering::Relaxed),
        );
    }
    out.family(
        "fenec_router_entry_clients_total",
        "counter",
        "What a router with --entry-token made of the Fenec-Client header -- the global entry's name for its client -- of the requests no other router relayed, by outcome: named (the mark checked, the address the request's), bad_mark (not the entry's mark: another token, or a client sending the header itself), stale (the entry's mark, made more than 60 s from this router's clock), absent (a tenant's request carrying none: a client reaching the router itself, or an entry with no ENTRY_TOKEN). All but named count by the connection's address.",
    );
    for (i, outcome) in ENTRY_CLIENT.iter().enumerate() {
        let n: u64 = SHARDS
            .iter()
            .map(|s| s.entry[i].load(Ordering::Relaxed))
            .sum();
        out.sample(
            "fenec_router_entry_clients_total",
            &[("outcome", outcome)],
            n,
        );
    }
    out.family(
        "fenec_router_read_replicas_given_up",
        "gauge",
        "Read replicas whose copy was lost and made again three times in a row -- it refuses its primary, of a lineage the router could not learn -- and is made again no more: placed again by an operator.",
    );
    out.sample(
        "fenec_router_read_replicas_given_up",
        &[],
        GIVEN_UP.load(Ordering::Relaxed).max(0) as u64,
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
    ); // The exporter's, when the router traces (`--otlp-endpoint`).
    fenec_http::trace::metrics(out);
}
