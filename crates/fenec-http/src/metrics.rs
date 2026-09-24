//! `GET /_metrics`: what the server has done and what it holds, in
//! Prometheus's text format.
//!
//! A statement is counted from its arrival to its answer -- the wait for the
//! lock and, under `--sync always`, for the disk included, since that is the
//! latency a client sees -- by transport (`pg`, `http`) and by whether it
//! wrote. The counters are process-wide atomics, spread over shards a
//! thread each: every connection is a thread, and eight of them counting
//! into one set of counters cost a statement 720 ns in a tight loop, where
//! one alone costs 6.4 ns -- the cache line went from core to core. With a
//! shard each, eight cost 6.7 ns. A thread takes the next shard as it
//! starts, and a scrape adds them up.
//!
//! The path carries an underscore, as `/_admin` and `/_replication` do: a
//! collection may well be called `metrics`, and `GET /metrics` is how its
//! rows are read. Prometheus takes the path as `metrics_path`.
//!
//! The gauges are read under the database's shared lock, which a scrape
//! holds as briefly as a `count` does. The counters are one set per process
//! -- `fenec-pg` serves one database, or one directory of tenants, per
//! process -- and a tenant node counts its tenants, not what they hold: a
//! tenant's collection names are not the node's to publish.

use crate::http::{Request, Response};
use crate::replication::Replication;
use crate::tenants::Stats;
use crate::{constant_eq, Config};
use fenec_core::prelude::*;
use std::cell::Cell;
use std::fmt::{Display, Write as _};
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Where a statement came in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Pg = 0,
    Http = 1,
}

impl Transport {
    fn name(self) -> &'static str {
        match self {
            Transport::Pg => "pg",
            Transport::Http => "http",
        }
    }
}

/// Upper bounds of the latency buckets, in microseconds: 100 µs, where a
/// point read over HTTP lands, to 10 s, where a full rebuild does.
const BUCKETS: [u64; 16] = [
    100, 250, 500, 1_000, 2_500, 5_000, 10_000, 25_000, 50_000, 100_000, 250_000, 500_000,
    1_000_000, 2_500_000, 5_000_000, 10_000_000,
];

/// One transport's statements of one kind. A bucket counts only its own
/// range; the exposition adds them up, as Prometheus's buckets are
/// cumulative.
struct Series {
    count: AtomicU64,
    errors: AtomicU64,
    micros: AtomicU64,
    buckets: [AtomicU64; BUCKETS.len()],
}

#[allow(clippy::declare_interior_mutable_const)]
const ZERO: AtomicU64 = AtomicU64::new(0);
#[allow(clippy::declare_interior_mutable_const)]
const SERIES: Series = Series {
    count: ZERO,
    errors: ZERO,
    micros: ZERO,
    buckets: [ZERO; BUCKETS.len()],
};

#[allow(clippy::declare_interior_mutable_const)]
const ROW: [Series; 2] = [SERIES; 2];

/// One thread's counters -- a few threads' past sixteen. Aligned so that no
/// two shards share a cache line (128 bytes: Apple's cores fetch two lines
/// of 64 at once).
#[repr(align(128))]
struct Shard {
    /// `[transport][wrote]`.
    statements: [[Series; 2]; 2],
    slow: [AtomicU64; 2],
}

#[allow(clippy::declare_interior_mutable_const)]
const SHARD: Shard = Shard {
    statements: [ROW; 2],
    slow: [ZERO; 2],
};

static SHARDS: [Shard; 16] = [SHARD; 16];
static NEXT_SHARD: AtomicUsize = AtomicUsize::new(0);
static CONNECTIONS: [AtomicI64; 2] = [AtomicI64::new(0), AtomicI64::new(0)];
/// `--slow-ms` in microseconds; 0 is off.
static SLOW_MICROS: AtomicU64 = AtomicU64::new(0);
static STARTED: OnceLock<u64> = OnceLock::new();

thread_local! {
    /// Whether the statement this thread is running wrote. A connection is
    /// a thread and runs one statement at a time, so the flag is set where
    /// the write lock is taken and read where the statement is counted,
    /// with no signature between the two having to carry it.
    static WROTE: Cell<bool> = const { Cell::new(false) };
    /// This thread's shard, handed out in turn.
    static MINE: usize = NEXT_SHARD.fetch_add(1, Ordering::Relaxed) % SHARDS.len();
}

/// Notes that the statement this thread is running writes.
pub fn wrote() {
    WROTE.with(|w| w.set(true));
}

/// Statements that took `ms` or longer are logged with their text: `--slow-ms`.
pub fn set_slow(ms: u64) {
    SLOW_MICROS.store(ms.saturating_mul(1000), Ordering::Relaxed);
}

/// The process's start, for `fenec_start_time_seconds`. Called when a
/// server starts; the first call wins.
pub fn started() {
    STARTED.get_or_init(now_secs);
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Counts a statement that took `took`, and logs it when it is slow. `text`
/// is asked for only then.
pub fn record(t: Transport, took: Duration, failed: bool, text: impl FnOnce() -> String) {
    let wrote = WROTE.with(|w| w.replace(false));
    let shard = &SHARDS[MINE.with(|m| *m)];
    let s = &shard.statements[t as usize][wrote as usize];
    let micros = took.as_micros().min(u64::MAX as u128) as u64;
    s.count.fetch_add(1, Ordering::Relaxed);
    s.micros.fetch_add(micros, Ordering::Relaxed);
    if failed {
        s.errors.fetch_add(1, Ordering::Relaxed);
    }
    if let Some(b) = BUCKETS.iter().position(|&le| micros <= le) {
        s.buckets[b].fetch_add(1, Ordering::Relaxed);
    }
    let slow = SLOW_MICROS.load(Ordering::Relaxed);
    if slow > 0 && micros >= slow {
        shard.slow[t as usize].fetch_add(1, Ordering::Relaxed);
        let mut text = text();
        // A statement can be a bulk `put` of megabytes; the log wants what
        // it was, not all of it.
        if text.len() > 1000 {
            let mut end = 1000;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            text.push_str("...");
        }
        crate::log!(
            "slow statement: {:.1} ms, {} {}{}: {}",
            micros as f64 / 1000.0,
            t.name(),
            if wrote { "write" } else { "read" },
            if failed { ", failed" } else { "" },
            text.replace('\n', " ")
        );
    }
}

/// A connection, counted open for as long as this lives.
pub struct Connection(Transport);

impl Connection {
    pub fn open(t: Transport) -> Connection {
        CONNECTIONS[t as usize].fetch_add(1, Ordering::Relaxed);
        Connection(t)
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        CONNECTIONS[self.0 as usize].fetch_sub(1, Ordering::Relaxed);
    }
}

/// What a scrape reports on besides the counters.
pub(crate) enum Source<'a> {
    Single {
        db: &'a std::sync::RwLock<Database>,
        repl: Option<&'a Replication>,
    },
    Tenants(Stats),
}

/// The server's token or the admin token may read the metrics; with neither
/// and no JSON Web Tokens, the server is open and so are they. A JWT is not
/// taken: a scoped user must not learn the other collections' names.
pub(crate) fn handle(cfg: &Config, req: &Request, source: Source) -> Response {
    if !allowed(cfg, req, false) {
        return Response::error(401, "invalid or missing token")
            .header("WWW-Authenticate", "Bearer");
    }
    Response {
        status: 200,
        body: render(source).into_bytes(),
        content_type: "text/plain; version=0.0.4; charset=utf-8",
        extra: Vec::new(),
    }
}

/// Whether `req` may read what the node counts: its server token or the
/// admin token, or anyone on a server with neither and no JSON Web Tokens.
/// `admin` asks for the admin token alone, as a tenant node's statements
/// do: they name the tenants' collections. A JWT is never taken: a scoped
/// user must not learn the other collections' names.
pub(crate) fn allowed(cfg: &Config, req: &Request, admin: bool) -> bool {
    let tokens = match admin {
        true => [None, cfg.admin_token.as_deref()],
        false => [cfg.token.as_deref(), cfg.admin_token.as_deref()],
    };
    let open = !admin
        && [cfg.token.as_deref(), cfg.admin_token.as_deref()]
            .iter()
            .all(Option::is_none)
        && cfg.access.is_none();
    let given = req
        .header("authorization")
        .and_then(|v| v.strip_prefix("Bearer "));
    open || given.is_some_and(|g| {
        tokens
            .iter()
            .flatten()
            .any(|t| constant_eq(g.as_bytes(), t.as_bytes()))
    })
}

/// The exposition: counters, then what the source holds.
fn render(source: Source) -> String {
    let mut out = Text(String::with_capacity(8 << 10));
    out.family(
        "fenec_build_info",
        "gauge",
        "The version running, as a label.",
    );
    out.sample("fenec_build_info", &[("version", fenec_core::VERSION)], 1);
    out.family(
        "fenec_start_time_seconds",
        "gauge",
        "When the process started, in seconds since the epoch.",
    );
    out.sample(
        "fenec_start_time_seconds",
        &[],
        *STARTED.get_or_init(now_secs),
    );

    let kinds = |t: usize, w: usize| {
        [
            ("transport", [Transport::Pg, Transport::Http][t].name()),
            ("kind", ["read", "write"][w]),
        ]
    };
    out.family(
        "fenec_statements_total",
        "counter",
        "Statements answered, by transport and by whether they wrote.",
    );
    each(|t, w| {
        let n = sum(t, w, |s| &s.count);
        out.sample("fenec_statements_total", &kinds(t, w), n)
    });
    out.family(
        "fenec_statement_errors_total",
        "counter",
        "Statements answered with an error, the client's or the server's.",
    );
    each(|t, w| {
        let n = sum(t, w, |s| &s.errors);
        out.sample("fenec_statement_errors_total", &kinds(t, w), n)
    });
    out.family(
        "fenec_statement_duration_seconds",
        "histogram",
        "From a statement's arrival to its answer: lock waits and, under --sync always, the disk included.",
    );
    each(|t, w| {
        let [a, b] = kinds(t, w);
        let mut below = 0;
        for (i, le) in BUCKETS.iter().enumerate() {
            below += sum(t, w, |s| &s.buckets[i]);
            let le = format!("{}", *le as f64 / 1e6);
            out.sample(
                "fenec_statement_duration_seconds_bucket",
                &[a, b, ("le", &le)],
                below,
            );
        }
        let count = sum(t, w, |s| &s.count);
        out.sample(
            "fenec_statement_duration_seconds_bucket",
            &[a, b, ("le", "+Inf")],
            count,
        );
        out.sample(
            "fenec_statement_duration_seconds_sum",
            &[a, b],
            sum(t, w, |s| &s.micros) as f64 / 1e6,
        );
        out.sample("fenec_statement_duration_seconds_count", &[a, b], count);
    });
    out.family(
        "fenec_slow_statements_total",
        "counter",
        "Statements over --slow-ms, each also logged with its text.",
    );
    for t in 0..2 {
        let n: u64 = SHARDS.iter().map(|s| load(&s.slow[t])).sum();
        out.sample(
            "fenec_slow_statements_total",
            &[("transport", [Transport::Pg, Transport::Http][t].name())],
            n,
        );
    }
    out.family("fenec_connections", "gauge", "Connections open now.");
    for (t, n) in CONNECTIONS.iter().enumerate() {
        out.sample(
            "fenec_connections",
            &[("transport", [Transport::Pg, Transport::Http][t].name())],
            n.load(Ordering::Relaxed),
        );
    }

    match source {
        Source::Single { db, repl } => {
            let g = db.read().unwrap_or_else(|e| e.into_inner());
            let stats = g.stats();
            out.family("fenec_documents", "gauge", "Documents in a collection.");
            for c in &stats {
                out.sample("fenec_documents", &[("collection", &c.name)], c.documents);
            }
            out.family(
                "fenec_data_bytes",
                "gauge",
                "A collection's live records, in bytes.",
            );
            for c in &stats {
                out.sample("fenec_data_bytes", &[("collection", &c.name)], c.bytes);
            }
            out.family(
                "fenec_dead_bytes",
                "gauge",
                "A collection's overwritten and deleted records, which compact gives back.",
            );
            for c in &stats {
                out.sample("fenec_dead_bytes", &[("collection", &c.name)], c.dead_bytes);
            }
            out.family(
                "fenec_memory_bytes",
                "gauge",
                "The data footprint --max-memory holds to: records, indexes and graphs. Not RSS.",
            );
            out.sample("fenec_memory_bytes", &[], g.memory_bytes());
            out.family(
                "fenec_change_sequence",
                "gauge",
                "The last write's sequence number; a replica's trails its primary's by its lag.",
            );
            let seq = g.change_seq();
            out.sample("fenec_change_sequence", &[], seq);
            out.family(
                "fenec_storage_failed",
                "gauge",
                "1 once the disk refused a write: writes stop until the file is reopened.",
            );
            out.sample("fenec_storage_failed", &[], g.failure().is_some() as u8);
            unlinked(&mut out, g.unlinked());
            drop(g);
            if let Some(repl) = repl {
                repl.metrics(&mut out, seq);
            }
        }
        Source::Tenants(s) => {
            out.family("fenec_tenants", "gauge", "Tenants on disk.");
            out.sample("fenec_tenants", &[], s.tenants);
            out.family("fenec_tenants_open", "gauge", "Tenants open in memory.");
            out.sample("fenec_tenants_open", &[], s.open.len());
            out.family(
                "fenec_tenants_disk_bytes",
                "gauge",
                "Every tenant's file, together.",
            );
            out.sample("fenec_tenants_disk_bytes", &[], s.disk);
            out.family(
                "fenec_memory_bytes",
                "gauge",
                "The open tenants' data footprint, together. Not RSS.",
            );
            out.sample(
                "fenec_memory_bytes",
                &[],
                s.open.iter().map(|(_, m, _)| m).sum::<usize>(),
            );
            unlinked(&mut out, s.unlinked);
        }
    }
    out.0
}

/// How far linking the vectors an open left out of the graph has to go
/// (`link::beside`); 0 once it is done.
fn unlinked(out: &mut Text, n: usize) {
    out.family(
        "fenec_vectors_unlinked",
        "gauge",
        "Vectors the open left out of the graph: near measures each until they are linked.",
    );
    out.sample("fenec_vectors_unlinked", &[], n);
}

fn each(mut f: impl FnMut(usize, usize)) {
    for t in 0..2 {
        for w in 0..2 {
            f(t, w);
        }
    }
}

/// One counter of `[transport][wrote]`, over every shard. Shards are read
/// one after another while statements go on, so a total can be a statement
/// short of a bucket read a moment later -- as any scrape of a running
/// server can be.
fn sum(t: usize, w: usize, field: impl Fn(&Series) -> &AtomicU64) -> u64 {
    SHARDS
        .iter()
        .map(|s| load(field(&s.statements[t][w])))
        .sum()
}

fn load(n: &AtomicU64) -> u64 {
    n.load(Ordering::Relaxed)
}

/// Prometheus's text format, written by hand: one `# HELP` and `# TYPE`
/// per family, then its samples.
pub(crate) struct Text(String);

impl Text {
    pub(crate) fn family(&mut self, name: &str, kind: &str, help: &str) {
        let _ = writeln!(self.0, "# HELP {name} {help}\n# TYPE {name} {kind}");
    }

    pub(crate) fn sample(&mut self, name: &str, labels: &[(&str, &str)], value: impl Display) {
        self.0.push_str(name);
        for (i, (k, v)) in labels.iter().enumerate() {
            self.0.push(if i == 0 { '{' } else { ',' });
            self.0.push_str(k);
            self.0.push_str("=\"");
            for c in v.chars() {
                match c {
                    '\\' => self.0.push_str("\\\\"),
                    '"' => self.0.push_str("\\\""),
                    '\n' => self.0.push_str("\\n"),
                    c => self.0.push(c),
                }
            }
            self.0.push('"');
        }
        if !labels.is_empty() {
            self.0.push('}');
        }
        let _ = writeln!(self.0, " {value}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_are_written_as_prometheus_reads_them() {
        let mut t = Text(String::new());
        t.family("x_total", "counter", "What x counts.");
        t.sample("x_total", &[], 3);
        t.sample("x_total", &[("a", "b"), ("c", "d\"e\\f\ng")], 1.5);
        assert_eq!(
            t.0,
            "# HELP x_total What x counts.\n# TYPE x_total counter\n\
             x_total 3\n\
             x_total{a=\"b\",c=\"d\\\"e\\\\f\\ng\"} 1.5\n"
        );
    }

    #[test]
    fn a_bucket_holds_its_upper_bound() {
        // `le` is "less than or equal": a statement of exactly 1 ms is in the
        // 1 ms bucket, one a microsecond over it is not.
        let at = |micros: u64| BUCKETS.iter().position(|&le| micros <= le);
        assert_eq!(at(1_000), Some(3));
        assert_eq!(at(1_001), Some(4));
        assert_eq!(at(10_000_001), None);
    }
}
