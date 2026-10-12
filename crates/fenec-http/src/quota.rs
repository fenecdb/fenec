//! A tenant's quotas on a tenant node (`fenec-server --dir`): what one
//! tenant may take of a node its neighbours share.
//!
//! ```text
//! bytes        its file, as written                     507  at the landing
//! rows         the documents of its own collections     507  at the landing
//! streams      subscriptions, /_changes waits and       429  Retry-After
//!              Fenec-After waits
//! held         writes held until they can write         429  Retry-After
//!              (Fenec-Wait): a queue's waiting workers
//! requests     requests in flight                       429  Retry-After
//! writes       rows written a second, a token bucket    429  Retry-After
//! write_bytes  bytes written a second, a token bucket   429  Retry-After
//! ```
//!
//! **A 429 says its wait to the millisecond.** `Retry-After` is whole
//! seconds, as HTTP has it, rounded up; the body's `retry_after_ms` is the
//! wait itself, which a client prefers: a bucket of 600 writes a second
//! refills a write in 1.7 ms, and fifty couriers told `Retry-After: 1`
//! waited fifty seconds between them.
//!
//! **`writes` counts rows written, as they land.** A write is let through
//! while the bucket holds a row's worth, and charged the rows of its
//! collections it wrote as it lands -- in debt past the bucket, as the
//! bytes are -- so a claim that took nothing, a `require` unmet, a held
//! claim's every look and run that wrote nothing costs none: taken as a
//! write started, 45 claims held at once by a fleet of the Starter plan
//! were refused for `writes` before any for being held. Only a request's
//! own landings are charged: the sweep of rows past their `@ttl`, a
//! compact or a replica's apply is the node's, not the tenant's rate.
//!
//! **`held` is apart from `streams`.** A plan sizes how many workers may
//! wait at once apart from how many dashboards may stream: counted
//! together, forty couriers waiting for orders left a dispatcher's map no
//! subscription to open.
//!
//! **What is bounded is what is known exactly and cheaply.** A file's bytes
//! are the engine's count of what it appended, read with no system call;
//! rows are each store's count, a sum over the collections; streams and
//! requests in flight are counters a request takes and gives back; the
//! rates are a bucket each under a mutex. Each costs a tenant with no
//! quota nothing: the counters and the buckets are touched only where the
//! quota is set, and the engine is asked only where a byte, row or bytes-
//! a-second quota is (`Database::set_limit`).
//!
//! **Left out: memory, a statement's time, rows per collection.** What an
//! open tenant holds (`Database::memory_bytes`) counts what has been built,
//! and a read builds its indexes on first use -- a quota on it would refuse
//! a write because of a search, and the same data would hold less after a
//! restart; it follows the rows and the declared indexes, which the rows
//! quota and the schema bound, and `--max-memory` closes idle tenants for
//! the node as a whole. A statement's time would need the engine to stop a
//! running scan, which it cannot; requests in flight bound how many cores
//! one tenant's long reads take. Rows per collection is a tenant's own
//! design (`@ttl` on a log), not its plan's.
//!
//! **Bytes and rows are asked as a block lands**, its record built, so no
//! write that adds takes the file past its quota
//! (`fenec_core::engine::limit`); a block that only takes away -- deletes,
//! a drop -- always lands, and so does a `compact`, which only gives bytes
//! back. A file near its quota is compacted by the node once an eighth of
//! the quota is dead ([`crate::link::quota_policy`]), where a file held to
//! none waits for 64 MB.
//!
//! **The router enforces none of it.** It holds no keys and parses no
//! query, and a tenant lives whole on one node: the node's count is the
//! tenant's. The router keeps each tenant's quota in its directory and
//! sends it with the tenant to every node it places a copy on, so a move
//! or a failover finds it there.

use crate::http::Response;
use fenec_core::engine::{Landing, Limit};
use fenec_core::prelude::{Database, Value};
use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The header every quota refusal carries, naming the quota: a client
/// reads it without reading the body, and the node counts and logs the
/// refusal by it ([`noted`]).
pub const HEADER: &str = "Fenec-Quota";

/// What a quota bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Bytes,
    Rows,
    Streams,
    Requests,
    Writes,
    WriteBytes,
    Held,
}

pub const KINDS: [Kind; 7] = [
    Kind::Bytes,
    Kind::Rows,
    Kind::Streams,
    Kind::Requests,
    Kind::Writes,
    Kind::WriteBytes,
    Kind::Held,
];

impl Kind {
    /// Its name in JSON, a header, a metric's label and the audit log.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Bytes => "bytes",
            Kind::Rows => "rows",
            Kind::Streams => "streams",
            Kind::Requests => "requests",
            Kind::Writes => "writes",
            Kind::WriteBytes => "write_bytes",
            Kind::Held => "held",
        }
    }

    pub fn parse(name: &str) -> Option<Kind> {
        KINDS.into_iter().find(|k| k.name() == name)
    }

    fn at(self) -> usize {
        self as usize
    }
}

/// A tenant's quota, kind by kind; 0 is none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Quota([u64; 7]);

impl Quota {
    pub fn get(&self, k: Kind) -> u64 {
        self.0[k.at()]
    }

    pub fn with(mut self, k: Kind, v: u64) -> Quota {
        self.0[k.at()] = v;
        self
    }

    /// Whether it bounds nothing.
    pub fn is_none(&self) -> bool {
        self.0.iter().all(|&v| v == 0)
    }

    /// `{"bytes":0,...}`, every kind named, 0 for none.
    pub fn json(&self) -> String {
        let mut out = String::from("{");
        for (i, k) in KINDS.into_iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push('"');
            out.push_str(k.name());
            out.push_str("\":");
            out.push_str(&self.get(k).to_string());
        }
        out.push('}');
        out
    }
}

/// What is set for one tenant over its node's defaults: a kind it names is
/// its own -- 0 for none -- and one it leaves out, or names `null`, is the
/// node's. Kept beside the tenant's file as `<tenant>.quota`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Set([Option<u64>; 7]);

impl Set {
    pub fn get(&self, k: Kind) -> Option<u64> {
        self.0[k.at()]
    }

    pub fn with(mut self, k: Kind, v: Option<u64>) -> Set {
        self.0[k.at()] = v;
        self
    }

    pub fn is_empty(&self) -> bool {
        self.0.iter().all(Option::is_none)
    }

    /// A JSON object of kinds to whole numbers or `null`. A key that is no
    /// kind is refused rather than passed over: `"byte": 10` would set
    /// nothing, and the tenant would go on unbounded.
    pub fn parse(text: &str) -> Result<Set, String> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(Set::default());
        }
        let fields = fenec_core::json::parse_object(text)
            .map_err(|e| format!("a quota is a JSON object: {e}"))?;
        Set::from_members(fields)
    }

    /// [`Self::parse`] over an object read already: a router's create
    /// takes the quota as a member of its body.
    pub fn from_members(fields: Vec<(String, Value)>) -> Result<Set, String> {
        let mut set = Set::default();
        for (k, v) in fields {
            let Some(kind) = Kind::parse(&k) else {
                let names: Vec<&str> = KINDS.iter().map(|k| k.name()).collect();
                return Err(format!(
                    "`{k}` is no quota: {} (0 for none, null for the node's)",
                    names.join(", ")
                ));
            };
            set.0[kind.at()] = match v {
                Value::Null => None,
                Value::Int(n) if n >= 0 => Some(n as u64),
                other => {
                    return Err(format!(
                        "`{k}` is a whole number of 0 or more, or null, not {other:?}"
                    ))
                }
            };
        }
        Ok(set)
    }

    /// The kinds it names, `null` for none: `{}` sets nothing.
    pub fn json(&self) -> String {
        let mut out = String::from("{");
        for k in KINDS {
            if let Some(v) = self.get(k) {
                if out.len() > 1 {
                    out.push(',');
                }
                out.push('"');
                out.push_str(k.name());
                out.push_str("\":");
                out.push_str(&v.to_string());
            }
        }
        out.push('}');
        out
    }

    /// The quota this set makes of the node's `defaults`.
    pub fn over(&self, defaults: &Quota) -> Quota {
        let mut q = *defaults;
        for k in KINDS {
            if let Some(v) = self.get(k) {
                q = q.with(k, v);
            }
        }
        q
    }
}

/// A size as a flag gives one: bytes, or a number and `K`, `M`, `G` or `T`
/// (powers of 1024), `B` and `iB` allowed after.
pub fn size(text: &str) -> Option<u64> {
    let t = text.trim();
    let t = t
        .strip_suffix("iB")
        .or_else(|| t.strip_suffix('B'))
        .unwrap_or(t);
    let (digits, shift) = match t.char_indices().last() {
        Some((i, c)) if c.is_ascii_alphabetic() => {
            let shift = match c.to_ascii_uppercase() {
                'K' => 10,
                'M' => 20,
                'G' => 30,
                'T' => 40,
                _ => return None,
            };
            (&t[..i], shift)
        }
        _ => (t, 0),
    };
    digits.trim().parse::<u64>().ok()?.checked_mul(1 << shift)
}

/// A token bucket: `rate` a second, one second's worth at most.
#[derive(Default)]
struct Bucket {
    tokens: f64,
    at: Option<Instant>,
}

impl Bucket {
    fn refill(&mut self, rate: f64, now: Instant) {
        self.tokens = match self.at {
            Some(at) => {
                (self.tokens + rate * now.saturating_duration_since(at).as_secs_f64()).min(rate)
            }
            None => rate,
        };
        self.at = Some(now);
    }
}

/// One tenant's quota as it is held to it: the quota, and what of it its
/// requests hold now. Shared by the tenant and the engine's limit.
pub struct Held {
    limits: [AtomicU64; 7],
    requests: AtomicU64,
    streams: AtomicU64,
    held: AtomicU64,
    writes: Mutex<Bucket>,
    write_bytes: Mutex<Bucket>,
}

impl Held {
    pub fn new(q: Quota) -> Arc<Held> {
        Arc::new(Held {
            limits: q.0.map(AtomicU64::new),
            requests: AtomicU64::new(0),
            streams: AtomicU64::new(0),
            held: AtomicU64::new(0),
            writes: Mutex::new(Bucket::default()),
            write_bytes: Mutex::new(Bucket::default()),
        })
    }

    pub fn get(&self, k: Kind) -> u64 {
        self.limits[k.at()].load(Ordering::Relaxed)
    }

    pub fn quota(&self) -> Quota {
        Quota(std::array::from_fn(|i| {
            self.limits[i].load(Ordering::Relaxed)
        }))
    }

    /// Holds the tenant to `q` from now on. A bucket keeps what it holds,
    /// cut to the new rate's second at its next refill.
    pub fn set(&self, q: Quota) {
        for (slot, v) in self.limits.iter().zip(q.0) {
            slot.store(v, Ordering::Relaxed);
        }
    }

    /// The streams, requests and held writes it holds now.
    pub fn holding(&self) -> (u64, u64, u64) {
        (
            self.streams.load(Ordering::Relaxed),
            self.requests.load(Ordering::Relaxed),
            self.held.load(Ordering::Relaxed),
        )
    }

    /// A request in flight, or the 429 its quota answers. Counted only
    /// where the quota is set: a tenant with none takes no atomic.
    pub fn request(self: &Arc<Self>) -> Result<Slot, Response> {
        self.take(Kind::Requests, &self.requests, |max| {
            format!("the tenant has {max} requests in flight, its quota: retry shortly")
        })
    }

    /// A stream -- a subscription, a `/_changes` wait, a `Fenec-After`
    /// wait -- each a thread for as long as it lasts.
    pub fn stream(self: &Arc<Self>) -> Result<Slot, Response> {
        self.take(Kind::Streams, &self.streams, |max| {
            format!(
                "the tenant holds {max} streams -- subscriptions, waits for changes -- its \
                 quota: close one, or retry shortly"
            )
        })
    }

    /// A write held until it can write (`Fenec-Wait`): a worker waiting
    /// for a job, a thread for as long as it waits.
    pub fn held(self: &Arc<Self>) -> Result<Slot, Response> {
        self.take(Kind::Held, &self.held, |max| {
            format!(
                "the tenant holds {max} writes waiting to write, its quota: retry shortly, \
                 or wait for fewer"
            )
        })
    }

    fn take(
        self: &Arc<Self>,
        kind: Kind,
        count: &AtomicU64,
        why: impl Fn(u64) -> String,
    ) -> Result<Slot, Response> {
        let max = self.get(kind);
        if max == 0 {
            return Ok(Slot(None));
        }
        if count.fetch_add(1, Ordering::SeqCst) >= max {
            count.fetch_sub(1, Ordering::SeqCst);
            return Err(refusal(kind, 429, &why(max), Some(Duration::from_secs(1))));
        }
        Ok(Slot(Some((Arc::clone(self), kind))))
    }

    /// Whether a write may start now, by the tenant's rates: a row's worth
    /// of the rows written a second, and the bytes written a second paid
    /// for. Both are charged as the write lands ([`Self::charge_rows`],
    /// [`Self::charge`]) by what it wrote -- a write that wrote nothing
    /// costs nothing -- so one larger than a second's worth goes through
    /// and the next waits out its debt.
    pub fn write(&self) -> Result<(), Response> {
        self.write_at(Instant::now())
    }

    /// [`Self::write`] at `now`: a test's own clock.
    pub fn write_at(&self, now: Instant) -> Result<(), Response> {
        let rate = self.get(Kind::Writes);
        if rate > 0 {
            let mut b = self.writes.lock().unwrap_or_else(|e| e.into_inner());
            b.refill(rate as f64, now);
            if b.tokens < 1.0 {
                let wait = Duration::from_secs_f64((1.0 - b.tokens) / rate as f64);
                return Err(refusal(
                    Kind::Writes,
                    429,
                    &format!("the tenant writes past its quota of {rate} rows a second"),
                    Some(wait),
                ));
            }
        }
        let rate = self.get(Kind::WriteBytes);
        if rate > 0 {
            let mut b = self.write_bytes.lock().unwrap_or_else(|e| e.into_inner());
            b.refill(rate as f64, now);
            if b.tokens <= 0.0 {
                let wait = Duration::from_secs_f64(-b.tokens / rate as f64);
                return Err(refusal(
                    Kind::WriteBytes,
                    429,
                    &format!("the tenant writes past its quota of {rate} bytes a second"),
                    Some(wait),
                ));
            }
        }
        Ok(())
    }

    /// How long until a write may start, by the rates: `None` now.
    pub fn write_wait(&self) -> Option<Duration> {
        let now = Instant::now();
        let mut wait = None;
        let rate = self.get(Kind::Writes);
        if rate > 0 {
            let mut b = self.writes.lock().unwrap_or_else(|e| e.into_inner());
            b.refill(rate as f64, now);
            if b.tokens < 1.0 {
                wait = Some(Duration::from_secs_f64((1.0 - b.tokens) / rate as f64));
            }
        }
        let rate = self.get(Kind::WriteBytes);
        if rate > 0 {
            let mut b = self.write_bytes.lock().unwrap_or_else(|e| e.into_inner());
            b.refill(rate as f64, now);
            if b.tokens <= 0.0 {
                let w = Duration::from_secs_f64(-b.tokens / rate as f64 + 1e-6);
                wait = Some(wait.map_or(w, |o: Duration| o.max(w)));
            }
        }
        wait
    }

    /// The rows a write landed, against the rows a second.
    pub fn charge_rows(&self, rows: u64) {
        self.charge_rows_at(rows, Instant::now());
    }

    /// [`Self::charge_rows`] at `now`.
    pub fn charge_rows_at(&self, rows: u64, now: Instant) {
        let rate = self.get(Kind::Writes);
        if rate == 0 || rows == 0 {
            return;
        }
        let mut b = self.writes.lock().unwrap_or_else(|e| e.into_inner());
        b.refill(rate as f64, now);
        b.tokens -= rows as f64;
    }

    /// The bytes a write landed, against the bytes a second.
    pub fn charge(&self, bytes: u64) {
        self.charge_at(bytes, Instant::now());
    }

    /// [`Self::charge`] at `now`.
    pub fn charge_at(&self, bytes: u64, now: Instant) {
        let rate = self.get(Kind::WriteBytes);
        if rate == 0 || bytes == 0 {
            return;
        }
        let mut b = self.write_bytes.lock().unwrap_or_else(|e| e.into_inner());
        b.refill(rate as f64, now);
        b.tokens -= bytes as f64;
    }

    /// Whether any of its rates is set: what [`within`] holds a request to.
    pub fn rated(&self) -> bool {
        self.get(Kind::Writes) > 0 || self.get(Kind::WriteBytes) > 0
    }
}

/// What the engine holds `held`'s database to as each block lands, where
/// its bytes, rows, rows a second or bytes a second are bounded; `None`
/// asks nothing.
pub fn limit(held: &Arc<Held>) -> Option<Limit> {
    let q = held.quota();
    if [Kind::Bytes, Kind::Rows, Kind::Writes, Kind::WriteBytes]
        .iter()
        .all(|&k| q.get(k) == 0)
    {
        return None;
    }
    let held = Arc::clone(held);
    // A refusal names the quota and its bound, never what the tenant holds:
    // a scoped token's user is told the tenant is full, not how many rows
    // the other users' make.
    Some(Arc::new(move |db: &Database, l: &Landing| {
        if l.grows {
            let max = held.get(Kind::Bytes);
            if max > 0 && l.file + l.adds > max {
                return Err(format!(
                    "bytes: the write would take the tenant's file past its quota of {max} \
                     bytes: delete rows and compact, or raise the quota"
                ));
            }
            let max = held.get(Kind::Rows);
            if max > 0 && db.documents() > max {
                return Err(format!(
                    "rows: the write would take the tenant past its quota of {max} rows: \
                     delete rows, or raise the quota"
                ));
            }
        }
        held.charge(l.adds);
        // A request's own rows, not the sweep's, a compact's or a
        // replica's: those land on threads no request is held to a rate on.
        if writing_here() {
            held.charge_rows(l.writes);
        }
        Ok(())
    }))
}

/// A count a request holds of its tenant's quota -- a request in flight, a
/// stream -- given back as it is dropped. `Slot(None)` holds nothing.
pub struct Slot(Option<(Arc<Held>, Kind)>);

impl Drop for Slot {
    fn drop(&mut self) {
        if let Some((held, kind)) = &self.0 {
            let count = match kind {
                Kind::Streams => &held.streams,
                Kind::Held => &held.held,
                _ => &held.requests,
            };
            count.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

thread_local! {
    // The tenant a request on this thread writes to, where it is held to a
    // rate: a connection is a thread, and the write is found deep in
    // `handle`, where the tenant is not.
    static RATED: RefCell<Option<Arc<Held>>> = const { RefCell::new(None) };
}

/// Runs `f` with its writes held to `held`'s rates ([`writing`]). A tenant
/// with no rate leaves the thread as it is.
pub fn within<R>(held: &Arc<Held>, f: impl FnOnce() -> R) -> R {
    if !held.rated() {
        return f();
    }
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            RATED.with(|r| *r.borrow_mut() = None);
        }
    }
    RATED.with(|r| *r.borrow_mut() = Some(Arc::clone(held)));
    let _reset = Reset;
    f()
}

/// Whether the write this thread is about to make is let through by its
/// tenant's rates, asked where a write is known to be one: the 429 when it
/// is not.
pub fn writing() -> Result<(), Response> {
    RATED.with(|r| match &*r.borrow() {
        Some(held) => held.write(),
        None => Ok(()),
    })
}

/// Whether this thread is a request held to its tenant's rates: what a
/// landing charges its rows to.
fn writing_here() -> bool {
    RATED.with(|r| r.borrow().is_some())
}

/// A quota's refusal: `{"error", "quota", "retry_after", "retry_after_ms"}`,
/// the quota named in [`HEADER`] too, and `Retry-After` where coming back
/// helps: whole seconds there and in `retry_after`, as HTTP has them, the
/// wait itself in `retry_after_ms`.
pub fn refusal(kind: Kind, status: u16, why: &str, retry: Option<Duration>) -> Response {
    let mut body = String::from("{\"error\":");
    fenec_core::json::escape_into(&mut body, why);
    body.push_str(",\"quota\":\"");
    body.push_str(kind.name());
    body.push('"');
    let secs = retry.map(seconds);
    if let (Some(s), Some(d)) = (secs, retry) {
        body.push_str(&format!(
            ",\"retry_after\":{s},\"retry_after_ms\":{}",
            millis(d)
        ));
    }
    body.push('}');
    let resp = Response::json(status, body).header(HEADER, kind.name());
    match secs {
        Some(s) => resp.header("Retry-After", &s.to_string()),
        None => resp,
    }
}

/// `Retry-After` is whole seconds: the wait rounded up, a second at least.
fn seconds(d: Duration) -> u64 {
    let s = d.as_secs() + u64::from(d.subsec_nanos() > 0);
    s.max(1)
}

/// The wait in whole milliseconds, rounded up, one at least: come back
/// sooner and the bucket still refuses.
pub fn millis(d: Duration) -> u64 {
    let ms = d.as_millis() as u64 + u64::from(!d.subsec_nanos().is_multiple_of(1_000_000));
    ms.max(1)
}

/// The engine's refusal of a block past a quota (`Error::Quota`), whose
/// message names its kind first ([`limit`]): 507.
pub fn engine_refusal(msg: &str) -> Response {
    match split(msg) {
        Some((kind, why)) => refusal(kind, 507, why, None),
        None => Response::error(507, msg),
    }
}

/// `<kind>: <why>`, as [`limit`] words a refusal.
pub fn split(msg: &str) -> Option<(Kind, &str)> {
    let (k, why) = msg.split_once(": ")?;
    Some((Kind::parse(k)?, why))
}

/// Refusals by quota, for `/_metrics`: never by tenant, which is the
/// admin API's to tell (`GET /_admin/quotas`).
static REFUSED: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

pub fn refused(k: Kind) -> u64 {
    REFUSED[k.at()].load(Ordering::Relaxed)
}

/// Counts and logs `resp` where it is a quota's refusal: every one of them
/// passes here, whichever check made it.
pub fn noted(resp: &Response, tenant: &str, req: &crate::http::Request) {
    if resp.status != 429 && resp.status != 507 {
        return;
    }
    let Some(kind) = resp
        .extra
        .iter()
        .find(|(k, _)| k == HEADER)
        .and_then(|(_, v)| Kind::parse(v))
    else {
        return;
    };
    REFUSED[kind.at()].fetch_add(1, Ordering::Relaxed);
    if crate::audit::on() {
        use crate::audit::Field;
        let path = req.target.split('?').next().unwrap_or("");
        crate::audit::event(
            "quota",
            &[
                ("quota", Field::Text(kind.name())),
                ("tenant", Field::Text(tenant)),
                ("method", Field::Text(req.method.name())),
                ("path", Field::Text(path)),
                ("http_status", Field::Int(resp.status as u64)),
            ],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_set_reads_back_as_written_and_refuses_what_is_no_quota() {
        let s = Set::parse(r#"{"bytes": 1048576, "rows": 0, "streams": null}"#).unwrap();
        assert_eq!(s.get(Kind::Bytes), Some(1 << 20));
        assert_eq!(s.get(Kind::Rows), Some(0));
        assert_eq!(s.get(Kind::Streams), None);
        assert_eq!(s.json(), r#"{"bytes":1048576,"rows":0}"#);
        assert_eq!(Set::parse(&s.json()).unwrap(), s);
        assert!(Set::parse(r#"{"byte": 10}"#).is_err());
        assert!(Set::parse(r#"{"bytes": -1}"#).is_err());
        assert!(Set::parse(r#"{"bytes": 1.5}"#).is_err());
        let defaults = Quota::default().with(Kind::Rows, 10).with(Kind::Writes, 5);
        let q = s.over(&defaults);
        assert_eq!(q.get(Kind::Bytes), 1 << 20);
        assert_eq!(q.get(Kind::Rows), 0, "0 is the tenant's own: none");
        assert_eq!(q.get(Kind::Writes), 5, "left out, the node's");
    }

    #[test]
    fn sizes_read_as_flags_give_them() {
        assert_eq!(size("1048576"), Some(1 << 20));
        assert_eq!(size("64M"), Some(64 << 20));
        assert_eq!(size("64MiB"), Some(64 << 20));
        assert_eq!(size("2G"), Some(2 << 30));
        assert_eq!(size("10KB"), Some(10 << 10));
        assert_eq!(size("ten"), None);
        assert_eq!(size("5X"), None);
    }

    /// The count a slot holds is given back when it goes, and the one past
    /// the quota is refused with what a client needs to come back.
    #[test]
    fn requests_past_the_quota_are_refused_and_slots_given_back() {
        let held = Held::new(Quota::default().with(Kind::Requests, 2));
        let a = held.request().ok().unwrap();
        let _b = held.request().ok().unwrap();
        let refused = held.request().err().expect("the third is past the quota");
        assert_eq!(refused.status, 429);
        assert!(refused
            .extra
            .iter()
            .any(|(k, v)| k == "Retry-After" && v == "1"));
        assert!(refused
            .extra
            .iter()
            .any(|(k, v)| k == HEADER && v == "requests"));
        drop(a);
        assert_eq!(held.holding().1, 1);
        assert!(held.request().is_ok());
        assert_eq!(held.holding().1, 1, "the slot given back as it went");
        let none = Held::new(Quota::default());
        let _many: Vec<_> = (0..100).map(|_| none.request().ok().unwrap()).collect();
        assert_eq!(none.holding().1, 0, "no quota counts nothing");
    }

    fn retry_after(r: &Response) -> u64 {
        r.extra
            .iter()
            .find(|(k, _)| k == "Retry-After")
            .map(|(_, v)| v.parse().unwrap())
            .unwrap()
    }

    /// A write of a row's worth at the given moment: let through, and
    /// charged the rows it wrote as it lands.
    fn wrote(held: &Held, rows: u64, at: Instant) -> Result<(), Response> {
        held.write_at(at)?;
        held.charge_rows_at(rows, at);
        Ok(())
    }

    /// A rate of five rows a second lets five through at once and refuses
    /// the sixth with a wait of a fifth of a second: 200 ms in the body,
    /// rounded up to a second in `Retry-After`; a fifth of a second on, one
    /// more goes. A write that wrote nothing costs nothing. On a clock of
    /// the test's own: the machine's would have the refill depend on how
    /// fast it ran.
    #[test]
    fn a_rate_lets_its_second_through_and_then_refuses() {
        let held = Held::new(Quota::default().with(Kind::Writes, 5));
        let t0 = Instant::now();
        for _ in 0..100 {
            assert!(wrote(&held, 0, t0).is_ok(), "a write of no row is free");
        }
        for _ in 0..5 {
            assert!(wrote(&held, 1, t0).is_ok());
        }
        let r = held.write_at(t0).expect_err("the sixth is past the rate");
        assert_eq!(r.status, 429);
        let body = String::from_utf8_lossy(&r.body);
        assert!(body.contains(r#""quota":"writes""#), "{body}");
        assert!(
            body.contains(r#""retry_after":1,"retry_after_ms":200"#),
            "{body}"
        );
        assert_eq!(retry_after(&r), 1);
        let later = t0 + Duration::from_millis(200);
        assert!(wrote(&held, 1, later).is_ok());
        assert!(held.write_at(later).is_err());
        // A second's worth at most: idle for a minute, five again.
        let idle = t0 + Duration::from_secs(60);
        for _ in 0..5 {
            assert!(wrote(&held, 1, idle).is_ok());
        }
        assert!(held.write_at(idle).is_err());
        // A write of many rows goes through on a row's worth, and the
        // next waits out the debt: ten rows past five, 2.2 s.
        let again = idle + Duration::from_secs(60);
        assert!(wrote(&held, 15, again).is_ok());
        let r = held.write_at(again).expect_err("in debt");
        let body = String::from_utf8_lossy(&r.body);
        assert!(
            body.contains(r#""retry_after":3,"retry_after_ms":2200"#),
            "{body}"
        );
    }

    #[test]
    fn a_wait_is_whole_milliseconds_rounded_up() {
        assert_eq!(millis(Duration::from_micros(1_666)), 2);
        assert_eq!(millis(Duration::from_millis(1_700)), 1_700);
        assert_eq!(millis(Duration::ZERO), 1);
        assert_eq!(seconds(Duration::from_millis(1_700)), 2);
    }

    /// Bytes are charged as they land: a write larger than a second's
    /// worth goes through, and the next waits out its debt.
    #[test]
    fn bytes_a_second_are_charged_after_and_waited_out() {
        let held = Held::new(Quota::default().with(Kind::WriteBytes, 1000));
        let t0 = Instant::now();
        assert!(held.write_at(t0).is_ok());
        held.charge_at(3500, t0);
        let r = held.write_at(t0).expect_err("in debt");
        assert_eq!(retry_after(&r), 3, "2 500 bytes owed at 1 000 a second");
        assert!(held.write_at(t0 + Duration::from_millis(2400)).is_err());
        assert!(held.write_at(t0 + Duration::from_millis(2600)).is_ok());
    }

    #[test]
    fn an_engine_refusal_names_its_quota() {
        let r = engine_refusal("bytes: the write would take the file past 10 bytes");
        assert_eq!(r.status, 507);
        let body = String::from_utf8_lossy(&r.body);
        assert_eq!(
            body,
            r#"{"error":"the write would take the file past 10 bytes","quota":"bytes"}"#
        );
    }
}
