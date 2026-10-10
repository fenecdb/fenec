//! OpenTelemetry tracing: where a request's time goes, as spans an OTLP
//! collector takes -- Datadog's Agent, Grafana Tempo, Honeycomb, New Relic,
//! Elastic.
//!
//! **Context.** A request carrying W3C `traceparent` continues that trace,
//! and its `tracestate` goes on untouched; one carrying none starts a
//! trace of its own, sampled at `--trace-sample`. A sampled parent is
//! followed whatever the ratio (OpenTelemetry's `parentbased` sampler), so
//! a node behind a router traces what the router chose to and nothing
//! else. The router continues the client's trace and sends the node a
//! `traceparent` naming its own forward, so the two processes' spans are
//! one tree. `X-Request-Id` stays as it is, and rides on the server span as
//! `fenec.request_id`.
//!
//! **Spans.** Few, and each a place a request waits: the server span; the
//! wait for the database's lock; the statement's run; the durability wait
//! after it (`--sync always`'s fsync, and on a primary the wait for its
//! replicas' streams to send the write); a replica's wait for the write a
//! `Fenec-After` names; the router's forward to the node. A connection is
//! a thread serving one request at a time, so the trace being built is a
//! thread-local, as the request id is: set where the request is read, a
//! child added where a lock is taken, no signature in between carrying it.
//!
//! **Export.** OTLP over HTTP with JSON (`POST /v1/traces`,
//! `application/json`), written with the engine's JSON writer and a client
//! of a page: no protobuf, no gRPC, no dependency. A finished trace goes
//! into a bounded queue -- full, it is dropped and counted
//! (`fenec_trace_spans_dropped_total`), never waited for -- and a thread of
//! its own posts the queue every second, or as soon as a batch's worth is
//! in, each post bounded by a timeout. A request never waits on the
//! collector: an endpoint that is down, or that takes the connection and
//! never answers, costs the requests nothing and the spans their place.
//! fenecdb speaks no TLS, so the endpoint is a collector on the same host
//! or network -- the OpenTelemetry Collector, the Datadog Agent's OTLP
//! receiver -- which forwards over TLS.
//!
//! **Off, it is free.** Every hook asks one relaxed atomic first and
//! returns: a server started with no endpoint does what it did before,
//! within the noise of `make requests-bench`.

use crate::http::Request;
use crate::metrics::Text;
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The W3C header a trace's context comes in and goes out in.
pub const TRACEPARENT: &str = "traceparent";
/// The vendors' part of the context, passed on as it came.
pub const TRACESTATE: &str = "tracestate";

/// A trace holds at most this many spans; the rest are left out. A request
/// takes a handful, but a lock is asked for in loops a long `/batch` runs,
/// and a trace's memory is the request's until it ends.
const MAX_SPANS: usize = 64;
/// The longest `tracestate` passed on: the W3C bound (32 members of at most
/// 256 bytes each is more than any vendor sends; 512 is the spec's floor a
/// receiver must keep).
const MAX_STATE: usize = 512;

static ON: AtomicBool = AtomicBool::new(false);
/// Traces started here are kept when their id's low 56 bits fall under
/// this: OpenTelemetry's `TraceIdRatioBased`, so that every process
/// sampling at one ratio keeps the same traces.
static THRESHOLD: AtomicU64 = AtomicU64::new(0);
static EXPORTER: OnceLock<Exporter> = OnceLock::new();
static DROPPED_FULL: AtomicU64 = AtomicU64::new(0);
static DROPPED_FAILED: AtomicU64 = AtomicU64::new(0);
static EXPORTED: AtomicU64 = AtomicU64::new(0);
static KEY: OnceLock<u64> = OnceLock::new();
static THREADS: AtomicU64 = AtomicU64::new(0);

/// What a span is to the request: the request itself, a call out of the
/// process, or a part of the work inside it. OTLP's `SpanKind` numbers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Internal = 1,
    Server = 2,
    Client = 3,
}

/// An attribute's value.
#[derive(Clone, Debug)]
pub enum Val {
    Str(Cow<'static, str>),
    Int(i64),
    Bool(bool),
}

impl From<&'static str> for Val {
    fn from(s: &'static str) -> Val {
        Val::Str(Cow::Borrowed(s))
    }
}
impl From<String> for Val {
    fn from(s: String) -> Val {
        Val::Str(Cow::Owned(s))
    }
}
impl From<i64> for Val {
    fn from(n: i64) -> Val {
        Val::Int(n)
    }
}
impl From<u16> for Val {
    fn from(n: u16) -> Val {
        Val::Int(n as i64)
    }
}
impl From<bool> for Val {
    fn from(b: bool) -> Val {
        Val::Bool(b)
    }
}

/// A span as it is exported.
struct Rec {
    name: Cow<'static, str>,
    id: [u8; 8],
    parent: Option<[u8; 8]>,
    kind: Kind,
    /// Nanoseconds since the epoch.
    start: u64,
    end: u64,
    attrs: Vec<(&'static str, Val)>,
    /// OTLP's `STATUS_CODE_ERROR`, with its message.
    error: Option<String>,
}

/// A request's spans, the server span first, as the exporter takes them.
struct Trace {
    id: [u8; 16],
    state: Option<String>,
    spans: Vec<Rec>,
}

/// The trace the request this thread serves belongs to.
#[derive(Default)]
struct Ctx {
    /// A request is being served under a context: sampled or not, it has
    /// one to send on.
    active: bool,
    sampled: bool,
    id: [u8; 16],
    state: Option<String>,
    /// The span this process's request is, sampled or not: a router sends
    /// it on as the parent even when nothing is recorded.
    root: [u8; 8],
    parent: Option<[u8; 8]>,
    /// The request's start, on both clocks: children are timed by the
    /// monotonic one and placed on the wall clock from here.
    begun: Option<Instant>,
    begun_unix: u64,
    /// The spans, the server span first; a child's `end` is 0 while open.
    spans: Vec<Rec>,
    /// The children open now, innermost last: a new child's parent.
    open: Vec<usize>,
    /// Bumped at every request, so a guard outliving its request ends
    /// nothing of the next one.
    generation: u64,
}

thread_local! {
    static CTX: RefCell<Ctx> = RefCell::new(Ctx::default());
    /// The thread's number in the high 24 bits, its ids below.
    static NEXT: Cell<u64> = Cell::new(THREADS.fetch_add(1, Ordering::Relaxed) << 40);
    /// The connection this thread serves: where it was taken, and from whom.
    static PEER: Cell<Option<(SocketAddr, Option<SocketAddr>)>> = const { Cell::new(None) };
}

/// Whether tracing is on: an endpoint was given.
#[inline]
pub fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

fn key() -> u64 {
    *KEY.get_or_init(|| {
        let b = crate::crypto::random_bytes(8);
        u64::from_le_bytes(b[..8].try_into().unwrap_or([0; 8]))
    })
}

/// A fresh 64 bits: SplitMix64's finalizer over a key and the thread's
/// count, as a made request id is -- distinct within a process, as
/// unguessable as the key, no system call.
fn fresh() -> u64 {
    let n = NEXT.with(|n| {
        let v = n.get();
        n.set(v.wrapping_add(1));
        v
    });
    let mut z = n ^ key();
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    match z ^ (z >> 31) {
        0 => 1,
        z => z,
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64)
}

/// The threshold a ratio from 0 to 1 samples at.
fn threshold(ratio: f64) -> u64 {
    if ratio >= 1.0 {
        u64::MAX
    } else if ratio > 0.0 {
        (ratio * (1u64 << 56) as f64) as u64
    } else {
        0
    }
}

/// Whether a trace started here with id `id` is kept.
fn sampled(id: &[u8; 16]) -> bool {
    let t = THRESHOLD.load(Ordering::Relaxed);
    let low = u64::from_be_bytes(id[8..].try_into().unwrap_or([0; 8])) & ((1 << 56) - 1);
    t == u64::MAX || low < t
}

/// `traceparent`'s version 00: `00-<trace id>-<parent id>-<flags>`, each in
/// lower-case hex, neither id all zeros. A later version is read by the
/// same fields, as the spec has a receiver do; anything else is no context
/// at all, and the request starts a trace of its own.
pub fn parse_traceparent(v: &str) -> Option<([u8; 16], [u8; 8], bool)> {
    let v = v.trim();
    let b = v.as_bytes();
    if b.len() < 55 || (b.len() > 55 && b[55] != b'-') {
        return None;
    }
    let version = hex_byte(&b[0..2])?;
    if version == 0xff || (version == 0 && b.len() != 55) {
        return None;
    }
    if b[2] != b'-' || b[35] != b'-' || b[52] != b'-' {
        return None;
    }
    let mut id = [0u8; 16];
    for (i, o) in id.iter_mut().enumerate() {
        *o = hex_byte(&b[3 + 2 * i..5 + 2 * i])?;
    }
    let mut parent = [0u8; 8];
    for (i, o) in parent.iter_mut().enumerate() {
        *o = hex_byte(&b[36 + 2 * i..38 + 2 * i])?;
    }
    let flags = hex_byte(&b[53..55])?;
    if id == [0; 16] || parent == [0; 8] {
        return None;
    }
    Some((id, parent, flags & 1 == 1))
}

fn hex_byte(b: &[u8]) -> Option<u8> {
    let d = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    Some(d(b[0])? << 4 | d(b[1])?)
}

fn hex_into(out: &mut String, bytes: &[u8]) {
    for b in bytes {
        out.push(char::from(b"0123456789abcdef"[(b >> 4) as usize]));
        out.push(char::from(b"0123456789abcdef"[(b & 15) as usize]));
    }
}

/// The connection this thread serves from now on: `local` its address,
/// `peer` the client's. On the server span as `server.address` and
/// `client.address`.
pub fn connection(local: Option<SocketAddr>, peer: Option<SocketAddr>) {
    if !on() {
        return;
    }
    PEER.with(|p| p.set(local.map(|l| (l, peer))));
}

/// The route a request's path takes, as a template of few values --
/// `/t/{tenant}/{collection}/near` -- for `http.route` and the span's
/// name: a span named by the path itself would be a name per row.
pub fn route(path: &str) -> String {
    let mut out = String::with_capacity(32);
    let mut segs = path.split('/').filter(|s| !s.is_empty()).peekable();
    if segs.peek() == Some(&"t") {
        segs.next();
        out.push_str("/t");
        if segs.next().is_some() {
            out.push_str("/{tenant}");
        }
    }
    // Under a path of the server's own (`/_schema`, `/_admin`,
    // `/_changes`) the words it knows stay and a name -- a tenant, a
    // consumer -- is `{name}`; elsewhere the first name is a collection's.
    let mut own = false;
    let mut named = false;
    for (i, s) in segs.enumerate() {
        out.push('/');
        if i == 0 && s.starts_with('_') {
            own = true;
            out.push_str(s);
            continue;
        }
        let word = matches!(
            s,
            "query" | "batch" | "near" | "changes" | "all" | "collections"
        ) || own
            && matches!(
                s,
                "plan"
                    | "apply"
                    | "statements"
                    | "consumers"
                    | "tenants"
                    | "stats"
                    | "open"
                    | "lease"
                    | "schema"
                    | "freeze"
                    | "thaw"
                    | "file"
                    | "promote"
                    | "follow"
            );
        match (word, own, named) {
            (true, _, _) => out.push_str(s),
            (false, true, _) => out.push_str("{name}"),
            (false, false, false) => {
                out.push_str("{collection}");
                named = true;
            }
            (false, false, true) => out.push_str("{id}"),
        }
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

/// Starts the server span of `req` on this thread: continuing the trace its
/// `traceparent` names, or one of its own, sampled at the ratio. Anything
/// the thread held of a request before is let go.
pub fn begin(req: &Request) {
    if !on() {
        return;
    }
    CTX.with(|c| {
        let mut c = c.borrow_mut();
        c.generation = c.generation.wrapping_add(1);
        c.spans.clear();
        c.open.clear();
        let given = req.header(TRACEPARENT).and_then(parse_traceparent);
        let (id, parent, keep) = match given {
            Some((id, parent, flag)) => (id, Some(parent), flag),
            None => {
                let mut id = [0u8; 16];
                id[..8].copy_from_slice(&fresh().to_be_bytes());
                id[8..].copy_from_slice(&fresh().to_be_bytes());
                let keep = sampled(&id);
                (id, None, keep)
            }
        };
        c.active = true;
        c.sampled = keep;
        c.id = id;
        c.parent = parent;
        c.root = fresh().to_be_bytes();
        c.state = match (given, req.header(TRACESTATE)) {
            (Some(_), Some(s)) if !s.is_empty() && s.len() <= MAX_STATE => Some(s.to_string()),
            _ => None,
        };
        if !keep {
            return;
        }
        c.begun = Some(Instant::now());
        c.begun_unix = now_unix();
        let route = route(&req.path);
        let mut attrs: Vec<(&'static str, Val)> = Vec::with_capacity(16);
        attrs.push(("http.request.method", Val::from(req.method.name())));
        attrs.push(("url.path", Val::from(req.path.clone())));
        attrs.push(("url.scheme", Val::from("http")));
        attrs.push(("db.system", Val::from("fenecdb")));
        crate::request_id::with(|r| {
            if !r.is_empty() {
                attrs.push(("fenec.request_id", Val::from(r.to_string())));
            }
        });
        if let Some((local, peer)) = PEER.with(Cell::get) {
            attrs.push(("server.address", Val::from(local.ip().to_string())));
            attrs.push(("server.port", Val::Int(local.port() as i64)));
            if let Some(p) = peer {
                attrs.push(("client.address", Val::from(p.ip().to_string())));
            }
        }
        let name = format!("{} {route}", req.method.name());
        attrs.push(("http.route", Val::from(route)));
        let (root, parent) = (c.root, c.parent);
        let start = c.begun_unix;
        c.spans.push(Rec {
            name: Cow::Owned(name),
            id: root,
            parent,
            kind: Kind::Server,
            start,
            end: 0,
            attrs,
            error: None,
        });
    });
}

/// Whether this thread's request is being recorded: worth working out an
/// attribute for.
#[inline]
pub fn recording() -> bool {
    on() && CTX.with(|c| c.borrow().sampled && c.borrow().active)
}

/// Sets an attribute of the server span.
pub fn attr(key: &'static str, val: impl Into<Val>) {
    if !on() {
        return;
    }
    CTX.with(|c| {
        let mut c = c.borrow_mut();
        if !(c.active && c.sampled) {
            return;
        }
        if let Some(root) = c.spans.first_mut() {
            set(&mut root.attrs, key, val.into());
        }
    });
}

fn set(attrs: &mut Vec<(&'static str, Val)>, key: &'static str, val: Val) {
    match attrs.iter_mut().find(|(k, _)| *k == key) {
        Some(slot) => slot.1 = val,
        None => attrs.push((key, val)),
    }
}

/// Ends this thread's request, answered with `status`, and hands its spans
/// to the exporter.
pub fn end(status: u16) {
    if !on() {
        return;
    }
    let trace = CTX.with(|c| {
        let mut c = c.borrow_mut();
        let was = std::mem::take(&mut c.active);
        if !(was && c.sampled) || c.spans.is_empty() {
            c.spans.clear();
            return None;
        }
        let now = elapsed_unix(&c);
        // A child still open -- a guard held past the answer -- ends with it.
        for i in std::mem::take(&mut c.open) {
            if c.spans[i].end == 0 {
                c.spans[i].end = now;
            }
        }
        let root = &mut c.spans[0];
        root.end = now;
        set(
            &mut root.attrs,
            "http.response.status_code",
            Val::Int(status as i64),
        );
        // A server span is an error for the server's own failures alone: a
        // 4xx is the client's (OpenTelemetry's HTTP conventions).
        if status >= 500 {
            root.error = Some(format!("{status}"));
            set(&mut root.attrs, "error.type", Val::from(status.to_string()));
        }
        Some(Trace {
            id: c.id,
            state: c.state.take(),
            spans: std::mem::take(&mut c.spans),
        })
    });
    if let (Some(t), Some(e)) = (trace, EXPORTER.get()) {
        e.push(t);
    }
}

/// Lets this thread's request go without a span: one that takes the
/// connection over -- a subscription, a replica's stream -- would hold its
/// spans for as long as it lasts.
pub fn discard() {
    if !on() {
        return;
    }
    CTX.with(|c| {
        let mut c = c.borrow_mut();
        c.active = false;
        c.spans.clear();
        c.open.clear();
    });
}

fn elapsed_unix(c: &Ctx) -> u64 {
    match c.begun {
        Some(b) => c.begun_unix + b.elapsed().as_nanos() as u64,
        None => now_unix(),
    }
}

/// A child span of this thread's request, open until the guard is dropped.
/// Off, or with no request recorded, it is nothing and costs one atomic.
#[must_use = "the span ends when the guard is dropped"]
pub struct Span {
    at: usize,
    generation: u64,
    live: bool,
}

/// A child span of the request this thread serves, of kind `Internal`.
#[inline]
pub fn span(name: &'static str) -> Span {
    span_of(name, Kind::Internal)
}

/// A child span of `kind`.
#[inline]
pub fn span_of(name: &'static str, kind: Kind) -> Span {
    if !on() {
        return Span {
            at: 0,
            generation: 0,
            live: false,
        };
    }
    open_span(name, kind)
}

#[cold]
fn open_span(name: &'static str, kind: Kind) -> Span {
    CTX.with(|c| {
        let mut c = c.borrow_mut();
        if !(c.active && c.sampled) || c.spans.is_empty() || c.spans.len() >= MAX_SPANS {
            return Span {
                at: 0,
                generation: 0,
                live: false,
            };
        }
        let parent = match c.open.last() {
            Some(&i) => c.spans[i].id,
            None => c.root,
        };
        let start = elapsed_unix(&c);
        let at = c.spans.len();
        c.spans.push(Rec {
            name: Cow::Borrowed(name),
            id: fresh().to_be_bytes(),
            parent: Some(parent),
            kind,
            start,
            end: 0,
            attrs: Vec::new(),
            error: None,
        });
        c.open.push(at);
        Span {
            at,
            generation: c.generation,
            live: true,
        }
    })
}

impl Span {
    /// Sets an attribute of this span.
    pub fn attr(&self, key: &'static str, val: impl Into<Val>) {
        if !self.live {
            return;
        }
        self.with(|r| set(&mut r.attrs, key, val.into()));
    }

    /// Marks this span failed, with why.
    pub fn error(&self, why: impl Into<String>) {
        if !self.live {
            return;
        }
        let why = why.into();
        self.with(|r| r.error = Some(why));
    }

    /// Takes this span back out, as if it had never been opened: for a
    /// call that turned out not to be what the span named.
    pub fn forget(mut self) {
        if !self.live {
            return;
        }
        self.live = false;
        CTX.with(|c| {
            let mut c = c.borrow_mut();
            if c.generation != self.generation || !c.active || c.spans.len() != self.at + 1 {
                return;
            }
            c.spans.pop();
            if let Some(i) = c.open.iter().rposition(|&i| i == self.at) {
                c.open.remove(i);
            }
        });
    }

    fn with(&self, f: impl FnOnce(&mut Rec)) {
        CTX.with(|c| {
            let mut c = c.borrow_mut();
            if c.generation == self.generation && c.active {
                if let Some(r) = c.spans.get_mut(self.at) {
                    f(r);
                }
            }
        });
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        if !self.live {
            return;
        }
        CTX.with(|c| {
            let mut c = c.borrow_mut();
            if c.generation != self.generation || !c.active {
                return;
            }
            let now = elapsed_unix(&c);
            if let Some(r) = c.spans.get_mut(self.at) {
                r.end = now;
            }
            if let Some(i) = c.open.iter().rposition(|&i| i == self.at) {
                c.open.remove(i);
            }
        });
    }
}

/// The `traceparent` (and `tracestate`) to send a request out of this
/// thread's with: the innermost open span as the parent, the trace's
/// flag. `None` when no context is active -- tracing off, or no request.
pub fn outgoing(f: impl FnOnce(&str, Option<&str>)) {
    if !on() {
        return;
    }
    CTX.with(|c| {
        let c = c.borrow();
        if !c.active {
            return;
        }
        let parent = match c.open.last() {
            Some(&i) if c.sampled => c.spans[i].id,
            _ => c.root,
        };
        let mut v = String::with_capacity(55);
        v.push_str("00-");
        hex_into(&mut v, &c.id);
        v.push('-');
        hex_into(&mut v, &parent);
        v.push_str(if c.sampled { "-01" } else { "-00" });
        f(&v, c.state.as_deref());
    });
}

/// Whether a request out of this thread's carries a context of its own --
/// so a router leaves the client's `traceparent` out of what it forwards.
pub fn propagating() -> bool {
    on() && CTX.with(|c| c.borrow().active)
}

// ------------------------------------------------------------------ export

/// How the exporter is set up: `--otlp-endpoint`, `--otlp-header`,
/// `--trace-sample` and the standard `OTEL_*` variables.
#[derive(Default, Clone, Debug)]
pub struct Options {
    pub endpoint: Option<String>,
    pub headers: Vec<(String, String)>,
    pub sample: Option<f64>,
}

impl Options {
    /// `--otlp-header k=v`.
    pub fn header(&mut self, kv: &str) -> std::result::Result<(), String> {
        match kv.split_once('=') {
            Some((k, v)) if !k.trim().is_empty() && header_safe(k) && header_safe(v) => {
                self.headers
                    .push((k.trim().to_string(), v.trim().to_string()));
                Ok(())
            }
            _ => Err(format!("--otlp-header expects name=value, got `{kv}`")),
        }
    }

    /// `--trace-sample <ratio>`.
    pub fn sample(&mut self, v: &str) -> std::result::Result<(), String> {
        match v.parse::<f64>() {
            Ok(r) if (0.0..=1.0).contains(&r) => {
                self.sample = Some(r);
                Ok(())
            }
            _ => Err(format!(
                "--trace-sample expects a ratio from 0 to 1, got `{v}`"
            )),
        }
    }

    /// The settings these options and the environment make, or `None` when
    /// no endpoint is named anywhere: tracing stays off.
    pub fn settings(self, service: &str) -> std::result::Result<Option<Settings>, String> {
        self.settings_from(service, &|k| std::env::var(k).ok())
    }

    fn settings_from(
        self,
        service: &str,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> std::result::Result<Option<Settings>, String> {
        let env = |k: &str| env(k).filter(|v| !v.trim().is_empty());
        if env("OTEL_SDK_DISABLED").is_some_and(|v| v.trim().eq_ignore_ascii_case("true")) {
            return Ok(None);
        }
        // The flag, then the traces' own variable, both taken as they are
        // unless they name no path; then the general one, a base under
        // which `/v1/traces` goes, as the spec has it.
        let url = match (self.endpoint, env("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")) {
            (Some(u), _) | (None, Some(u)) => u,
            (None, None) => match env("OTEL_EXPORTER_OTLP_ENDPOINT") {
                Some(base) => format!("{}/v1/traces", base.trim_end_matches('/')),
                None => {
                    if self.sample.is_some_and(|r| r > 0.0) {
                        return Err(
                            "--trace-sample needs --otlp-endpoint: where would the spans go?"
                                .into(),
                        );
                    }
                    return Ok(None);
                }
            },
        };
        let endpoint = Endpoint::parse(&url)?;
        let mut headers = Vec::new();
        for var in [
            "OTEL_EXPORTER_OTLP_HEADERS",
            "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
        ] {
            if let Some(v) = env(var) {
                for kv in v.split(',').filter(|kv| !kv.trim().is_empty()) {
                    match kv.split_once('=') {
                        Some((k, v)) => {
                            let (k, v) = (percent_decoded(k.trim()), percent_decoded(v.trim()));
                            if !header_safe(&k) || !header_safe(&v) {
                                return Err(format!("{var}: `{kv}` cannot be a header"));
                            }
                            headers.retain(|(h, _): &(String, String)| !h.eq_ignore_ascii_case(&k));
                            headers.push((k, v));
                        }
                        None => return Err(format!("{var} expects name=value pairs, got `{kv}`")),
                    }
                }
            }
        }
        for (k, v) in self.headers {
            headers.retain(|(h, _): &(String, String)| !h.eq_ignore_ascii_case(&k));
            headers.push((k, v));
        }
        let mut resource = vec![("service.name".to_string(), service.to_string())];
        if let Some(v) = env("OTEL_RESOURCE_ATTRIBUTES") {
            for kv in v.split(',') {
                if let Some((k, v)) = kv.split_once('=') {
                    let (k, v) = (percent_decoded(k.trim()), percent_decoded(v.trim()));
                    if !k.is_empty() {
                        resource.retain(|(r, _)| *r != k);
                        resource.push((k, v));
                    }
                }
            }
        }
        if let Some(name) = env("OTEL_SERVICE_NAME") {
            resource.retain(|(r, _)| r != "service.name");
            resource.insert(0, ("service.name".to_string(), name.trim().to_string()));
        }
        resource.push(("service.version".into(), fenec_core::VERSION.into()));
        let ms = |k: &str, default: u64| -> std::result::Result<Duration, String> {
            match env(k) {
                None => Ok(Duration::from_millis(default)),
                Some(v) => v
                    .trim()
                    .parse::<u64>()
                    .map(Duration::from_millis)
                    .map_err(|_| format!("{k} expects milliseconds, got `{v}`")),
            }
        };
        let count = |k: &str, default: usize| -> std::result::Result<usize, String> {
            match env(k) {
                None => Ok(default),
                Some(v) => match v.trim().parse::<usize>() {
                    Ok(n) if n > 0 => Ok(n),
                    _ => Err(format!("{k} expects a count, got `{v}`")),
                },
            }
        };
        let timeout = match env("OTEL_EXPORTER_OTLP_TRACES_TIMEOUT") {
            Some(_) => ms("OTEL_EXPORTER_OTLP_TRACES_TIMEOUT", 10_000)?,
            None => ms("OTEL_EXPORTER_OTLP_TIMEOUT", 10_000)?,
        };
        let protocol = env("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL")
            .or_else(|| env("OTEL_EXPORTER_OTLP_PROTOCOL"));
        if let Some(p) = protocol.filter(|p| p.trim() != "http/json") {
            return Err(format!(
                "OTEL_EXPORTER_OTLP_PROTOCOL is `{p}`: fenecdb sends OTLP over HTTP as JSON \
                 alone (http/json, port 4318 on a collector)"
            ));
        }
        Ok(Some(Settings {
            endpoint,
            headers,
            sample: self.sample.unwrap_or(1.0),
            resource,
            queue: count("OTEL_BSP_MAX_QUEUE_SIZE", 2048)?,
            batch: count("OTEL_BSP_MAX_EXPORT_BATCH_SIZE", 512)?,
            delay: ms("OTEL_BSP_SCHEDULE_DELAY", 1000)?,
            timeout,
        }))
    }
}

/// A header name or value as it can go on a line: printable ASCII.
fn header_safe(s: &str) -> bool {
    s.bytes().all(|b| (0x20..0x7f).contains(&b))
}

/// `%XX` decoded, as `OTEL_EXPORTER_OTLP_HEADERS` values are written.
fn percent_decoded(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Where spans are posted: `http://host:port/path`.
#[derive(Clone, Debug)]
pub struct Endpoint {
    /// `host:port`, for the connection and the `Host` header.
    pub authority: String,
    pub path: String,
}

impl Endpoint {
    pub fn parse(url: &str) -> std::result::Result<Endpoint, String> {
        let url = url.trim();
        if url.starts_with("https://") {
            return Err(format!(
                "{url}: fenecdb speaks no TLS. Point it at a collector on this host or \
                 network -- the OpenTelemetry Collector, the Datadog Agent's OTLP receiver \
                 (http://localhost:4318) -- which forwards over TLS"
            ));
        }
        let rest = url
            .strip_prefix("http://")
            .ok_or_else(|| format!("{url}: the OTLP endpoint is an http:// URL"))?;
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        if authority.is_empty() || !header_safe(authority) || !header_safe(path) {
            return Err(format!("{url}: no host to send spans to"));
        }
        let authority = match authority.rsplit_once(':') {
            Some((_, p)) if p.parse::<u16>().is_ok() => authority.to_string(),
            _ => format!("{authority}:80"),
        };
        let path = match path {
            "" | "/" => "/v1/traces".to_string(),
            p => p.to_string(),
        };
        Ok(Endpoint { authority, path })
    }

    pub fn url(&self) -> String {
        format!("http://{}{}", self.authority, self.path)
    }
}

/// The exporter's settings.
#[derive(Clone, Debug)]
pub struct Settings {
    pub endpoint: Endpoint,
    pub headers: Vec<(String, String)>,
    /// The share of the traces that start here that are kept.
    pub sample: f64,
    /// `service.name` and the rest of the resource.
    pub resource: Vec<(String, String)>,
    /// Spans held for the exporter at most; past it they are dropped.
    pub queue: usize,
    /// Spans a post holds at most, and how many wake the exporter early.
    pub batch: usize,
    /// How often the queue is posted.
    pub delay: Duration,
    /// A post's bound: connecting, writing, and reading the answer.
    pub timeout: Duration,
}

/// Turns tracing on with `s`, once a process: the exporter's thread starts
/// and every request from now on is traced. A second call changes the
/// sampling ratio alone.
pub fn install(s: Settings) -> io::Result<()> {
    THRESHOLD.store(threshold(s.sample), Ordering::Relaxed);
    if EXPORTER.get().is_some() {
        return Ok(());
    }
    let _ = EXPORTER.set(Exporter {
        queue: Mutex::new(Queue::default()),
        wake: Condvar::new(),
        flushed: Condvar::new(),
        settings: s,
    });
    std::thread::Builder::new()
        .name("fenec-otlp".into())
        .spawn(|| {
            if let Some(e) = EXPORTER.get() {
                e.run();
            }
        })?;
    ON.store(true, Ordering::Relaxed);
    Ok(())
}

/// Changes the share of the traces started here that are kept.
pub fn set_sample(ratio: f64) {
    THRESHOLD.store(threshold(ratio), Ordering::Relaxed);
}

/// Posts what the queue holds now and waits for it, `within` at most: at a
/// shutdown, the last second's spans.
pub fn flush(within: Duration) {
    let Some(e) = EXPORTER.get() else {
        return;
    };
    let deadline = Instant::now() + within;
    let mut q = e.lock();
    q.flush = true;
    e.wake.notify_all();
    while q.spans > 0 || q.posting {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        q = e
            .flushed
            .wait_timeout(q, deadline - now)
            .unwrap_or_else(|p| p.into_inner())
            .0;
    }
}

/// The exporter's counts, in `/_metrics`.
pub fn metrics(out: &mut Text) {
    out.family(
        "fenec_trace_spans_exported_total",
        "counter",
        "Spans the OTLP endpoint took.",
    );
    out.sample(
        "fenec_trace_spans_exported_total",
        &[],
        EXPORTED.load(Ordering::Relaxed),
    );
    out.family(
        "fenec_trace_spans_dropped_total",
        "counter",
        "Spans left out: the exporter's queue was full, or the endpoint refused or did not answer.",
    );
    out.sample(
        "fenec_trace_spans_dropped_total",
        &[("reason", "queue_full")],
        DROPPED_FULL.load(Ordering::Relaxed),
    );
    out.sample(
        "fenec_trace_spans_dropped_total",
        &[("reason", "export_failed")],
        DROPPED_FAILED.load(Ordering::Relaxed),
    );
}

/// The counts `[exported, dropped as the queue was full, dropped as a post
/// failed]`.
pub fn counts() -> [u64; 3] {
    [
        EXPORTED.load(Ordering::Relaxed),
        DROPPED_FULL.load(Ordering::Relaxed),
        DROPPED_FAILED.load(Ordering::Relaxed),
    ]
}

#[derive(Default)]
struct Queue {
    traces: Vec<Trace>,
    spans: usize,
    flush: bool,
    posting: bool,
}

struct Exporter {
    queue: Mutex<Queue>,
    wake: Condvar,
    flushed: Condvar,
    settings: Settings,
}

impl Exporter {
    fn lock(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// A request's trace into the queue, or counted out of it. The lock is
    /// held for a push: the exporter takes the whole queue in one swap, so
    /// a request never waits on a post.
    fn push(&self, t: Trace) {
        let n = t.spans.len();
        let mut q = self.lock();
        if q.spans + n > self.settings.queue {
            drop(q);
            DROPPED_FULL.fetch_add(n as u64, Ordering::Relaxed);
            return;
        }
        q.spans += n;
        q.traces.push(t);
        let wake = q.spans >= self.settings.batch;
        drop(q);
        if wake {
            self.wake.notify_one();
        }
    }

    fn run(&self) {
        let mut conn: Option<BufReader<TcpStream>> = None;
        let mut body = String::new();
        let mut last_error: Option<Instant> = None;
        loop {
            let (traces, held) = {
                let mut q = self.lock();
                let until = Instant::now() + self.settings.delay;
                while q.spans < self.settings.batch && !q.flush {
                    let now = Instant::now();
                    if now >= until {
                        break;
                    }
                    q = self
                        .wake
                        .wait_timeout(q, until - now)
                        .unwrap_or_else(|p| p.into_inner())
                        .0;
                }
                q.flush = false;
                let held = std::mem::take(&mut q.spans);
                q.posting = held > 0;
                (std::mem::take(&mut q.traces), held)
            };
            if held == 0 {
                self.flushed.notify_all();
                continue;
            }
            // Posts of a batch's worth of spans at most, whole traces each.
            let mut from = 0;
            while from < traces.len() {
                let mut to = from;
                let mut spans = 0;
                while to < traces.len()
                    && (to == from || spans + traces[to].spans.len() <= self.settings.batch)
                {
                    spans += traces[to].spans.len();
                    to += 1;
                }
                body.clear();
                encode(&mut body, &self.settings.resource, &traces[from..to]);
                match self.post(&mut conn, body.as_bytes()) {
                    Ok(()) => {
                        EXPORTED.fetch_add(spans as u64, Ordering::Relaxed);
                    }
                    Err(e) => {
                        conn = None;
                        DROPPED_FAILED.fetch_add(spans as u64, Ordering::Relaxed);
                        // Once a minute at most: a collector that is down
                        // would write a line a second.
                        if last_error.is_none_or(|t| t.elapsed() >= Duration::from_secs(60)) {
                            last_error = Some(Instant::now());
                            crate::request_id::end();
                            crate::log!(
                                "tracing: could not post {spans} span(s) to {}: {e}",
                                self.settings.endpoint.url()
                            );
                        }
                    }
                }
                from = to;
            }
            drop(traces);
            let mut q = self.lock();
            q.posting = false;
            drop(q);
            self.flushed.notify_all();
        }
    }

    /// One post, on the connection kept from the last when there is one --
    /// and on a new one if that one was closed meanwhile.
    fn post(&self, conn: &mut Option<BufReader<TcpStream>>, body: &[u8]) -> io::Result<()> {
        let s = &self.settings;
        let mut head = String::with_capacity(256);
        for part in [
            "POST ",
            &s.endpoint.path,
            " HTTP/1.1\r\nHost: ",
            &s.endpoint.authority,
            "\r\nContent-Type: application/json\r\nUser-Agent: fenecdb/",
            fenec_core::VERSION,
            "\r\n",
        ] {
            head.push_str(part);
        }
        for (k, v) in &s.headers {
            for part in [k.as_str(), ": ", v.as_str(), "\r\n"] {
                head.push_str(part);
            }
        }
        head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
        let deadline = Instant::now() + s.timeout;
        if let Some(r) = conn.as_mut() {
            match exchange(r, head.as_bytes(), body, deadline) {
                Ok((status, keep)) => return answered(conn, status, keep),
                // Closed while idle: the collector never read the post.
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::UnexpectedEof
                            | io::ErrorKind::BrokenPipe
                            | io::ErrorKind::ConnectionReset
                    ) => {}
                Err(e) => return Err(e),
            }
        }
        let mut r = BufReader::new(connect(&s.endpoint.authority, s.timeout)?);
        let (status, keep) = exchange(&mut r, head.as_bytes(), body, deadline)?;
        *conn = Some(r);
        answered(conn, status, keep)
    }
}

fn answered(conn: &mut Option<BufReader<TcpStream>>, status: u16, keep: bool) -> io::Result<()> {
    if !keep {
        *conn = None;
    }
    match status {
        200..=299 => Ok(()),
        s => Err(io::Error::other(format!("answered {s}"))),
    }
}

fn connect(authority: &str, timeout: Duration) -> io::Result<TcpStream> {
    let mut last = io::Error::new(
        io::ErrorKind::NotFound,
        format!("{authority} does not resolve"),
    );
    for a in authority.to_socket_addrs()? {
        match TcpStream::connect_timeout(&a, timeout) {
            Ok(s) => {
                let _ = s.set_nodelay(true);
                return Ok(s);
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Sets a socket's timeouts to what is left before `deadline`.
fn bound(s: &TcpStream, deadline: Instant) -> io::Result<()> {
    let left = deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "the post timed out"))?;
    s.set_read_timeout(Some(left))?;
    s.set_write_timeout(Some(left))
}

/// Sends a post and reads its answer whole: the status, and whether the
/// connection may carry the next one.
fn exchange(
    r: &mut BufReader<TcpStream>,
    head: &[u8],
    body: &[u8],
    deadline: Instant,
) -> io::Result<(u16, bool)> {
    bound(r.get_ref(), deadline)?;
    {
        let mut w = r.get_ref();
        w.write_all(head)?;
        w.write_all(body)?;
        w.flush()?;
    }
    let mut line = String::new();
    bound(r.get_ref(), deadline)?;
    if r.read_line(&mut line)? == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    let status: u16 = line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| io::Error::other(format!("not an HTTP answer: {line:?}")))?;
    let (mut length, mut chunked, mut keep) = (None, false, true);
    let mut total = 0;
    loop {
        line.clear();
        bound(r.get_ref(), deadline)?;
        let n = r.read_line(&mut line)?;
        total += n;
        if n == 0 || total > crate::http::MAX_HEADER_BYTES {
            return Err(io::Error::other("the answer's head did not end"));
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            let (k, v) = (k.trim(), v.trim());
            if k.eq_ignore_ascii_case("content-length") {
                length = v.parse::<u64>().ok();
            } else if k.eq_ignore_ascii_case("transfer-encoding") {
                chunked = v.to_ascii_lowercase().contains("chunked");
            } else if k.eq_ignore_ascii_case("connection") {
                keep &= !v.eq_ignore_ascii_case("close");
            }
        }
    }
    // The body says nothing a count needs, but it is read out, so the
    // connection is where the next answer starts.
    if chunked {
        loop {
            line.clear();
            bound(r.get_ref(), deadline)?;
            if r.read_line(&mut line)? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            let size = u64::from_str_radix(line.trim().split(';').next().unwrap_or(""), 16)
                .map_err(|_| io::Error::other("a chunk's size is not hex"))?;
            bound(r.get_ref(), deadline)?;
            io::copy(&mut r.by_ref().take(size + 2), &mut io::sink())?;
            if size == 0 {
                break;
            }
        }
    } else if let Some(n) = length {
        bound(r.get_ref(), deadline)?;
        let read = io::copy(&mut r.by_ref().take(n), &mut io::sink())?;
        if read < n {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
    } else {
        // Neither: the body runs to the close.
        keep = false;
    }
    Ok((status, keep))
}

/// `ExportTraceServiceRequest` as OTLP's JSON mapping writes it: ids in
/// hex, 64-bit numbers as strings, enums as their numbers.
fn encode(out: &mut String, resource: &[(String, String)], traces: &[Trace]) {
    use fenec_core::json::escape_into;
    out.push_str(r#"{"resourceSpans":[{"resource":{"attributes":["#);
    for (i, (k, v)) in resource.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(r#"{"key":"#);
        escape_into(out, k);
        out.push_str(r#","value":{"stringValue":"#);
        escape_into(out, v);
        out.push_str("}}");
    }
    out.push_str(r#"]},"scopeSpans":[{"scope":{"name":"fenecdb","version":"#);
    escape_into(out, fenec_core::VERSION);
    out.push_str(r#"},"spans":["#);
    let mut first = true;
    for t in traces {
        for s in &t.spans {
            if !std::mem::take(&mut first) {
                out.push(',');
            }
            out.push_str(r#"{"traceId":""#);
            hex_into(out, &t.id);
            out.push_str(r#"","spanId":""#);
            hex_into(out, &s.id);
            out.push('"');
            if let Some(p) = &s.parent {
                out.push_str(r#","parentSpanId":""#);
                hex_into(out, p);
                out.push('"');
            }
            if let Some(state) = &t.state {
                out.push_str(r#","traceState":"#);
                escape_into(out, state);
            }
            out.push_str(r#","name":"#);
            escape_into(out, &s.name);
            out.push_str(r#","kind":"#);
            out.push_str(match s.kind {
                Kind::Internal => "1",
                Kind::Server => "2",
                Kind::Client => "3",
            });
            let end = s.end.max(s.start);
            out.push_str(&format!(
                r#","startTimeUnixNano":"{}","endTimeUnixNano":"{end}","attributes":["#,
                s.start
            ));
            for (i, (k, v)) in s.attrs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(r#"{"key":"#);
                escape_into(out, k);
                out.push_str(r#","value":{"#);
                match v {
                    Val::Str(s) => {
                        out.push_str(r#""stringValue":"#);
                        escape_into(out, s);
                    }
                    Val::Int(n) => out.push_str(&format!(r#""intValue":"{n}""#)),
                    Val::Bool(b) => out.push_str(if *b {
                        r#""boolValue":true"#
                    } else {
                        r#""boolValue":false"#
                    }),
                }
                out.push_str("}}");
            }
            out.push(']');
            match &s.error {
                Some(why) => {
                    out.push_str(r#","status":{"code":2,"message":"#);
                    escape_into(out, why);
                    out.push('}');
                }
                None => out.push_str(r#","status":{}"#),
            }
            out.push('}');
        }
    }
    out.push_str("]}]}]}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traceparent_is_read_as_the_spec_writes_it() {
        let tp = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let (id, parent, s) = parse_traceparent(tp).unwrap();
        assert_eq!(id[0], 0x4b);
        assert_eq!(parent[7], 0xb7);
        assert!(s);
        assert!(!parse_traceparent(&tp.replace("-01", "-00")).unwrap().2);
        for bad in [
            "",
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
            "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-x",
            "00-4bf92f3577b34da6a3ce929d0e0e4736x00f067aa0ba902b7-01",
        ] {
            assert!(parse_traceparent(bad).is_none(), "{bad}");
        }
        // A later version: its fields past these are not ours to read.
        assert!(parse_traceparent(&format!("01{}-what", &tp[2..])).is_some());
    }

    #[test]
    fn a_ratio_keeps_its_share_of_the_traces() {
        THRESHOLD.store(threshold(0.25), Ordering::Relaxed);
        let mut kept = 0;
        for _ in 0..40_000 {
            let mut id = [0u8; 16];
            id[..8].copy_from_slice(&fresh().to_be_bytes());
            id[8..].copy_from_slice(&fresh().to_be_bytes());
            kept += sampled(&id) as u32;
        }
        assert!((9_000..11_000).contains(&kept), "{kept}");
        THRESHOLD.store(threshold(0.0), Ordering::Relaxed);
        let id = [7u8; 16];
        assert!(!sampled(&id));
        THRESHOLD.store(threshold(1.0), Ordering::Relaxed);
        assert!(sampled(&id));
    }

    #[test]
    fn a_route_is_a_template_of_few_values() {
        assert_eq!(route("/query"), "/query");
        assert_eq!(route("/notes"), "/{collection}");
        assert_eq!(route("/notes/near"), "/{collection}/near");
        assert_eq!(route("/notes/all"), "/{collection}/all");
        assert_eq!(
            route("/t/acme/notes/changes"),
            "/t/{tenant}/{collection}/changes"
        );
        assert_eq!(route("/t/acme/batch"), "/t/{tenant}/batch");
        assert_eq!(route("/_schema/plan"), "/_schema/plan");
        assert_eq!(
            route("/_admin/tenants/acme/freeze"),
            "/_admin/tenants/{name}/freeze"
        );
        assert_eq!(
            route("/t/acme/_changes/consumers/sink"),
            "/t/{tenant}/_changes/consumers/{name}"
        );
        assert_eq!(route("/"), "/");
    }

    #[test]
    fn the_environment_is_read_as_opentelemetry_names_it() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        let none = Options::default().settings_from("s", &env(&[])).unwrap();
        assert!(none.is_none());
        assert!(Options {
            sample: Some(0.5),
            ..Options::default()
        }
        .settings_from("s", &env(&[]))
        .is_err());
        let s = Options::default()
            .settings_from(
                "fenec-server",
                &env(&[
                    ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://collector:4318/"),
                    ("OTEL_EXPORTER_OTLP_HEADERS", "dd-api-key=abc%3D,x=1"),
                    ("OTEL_SERVICE_NAME", "orders-db"),
                    ("OTEL_RESOURCE_ATTRIBUTES", "deployment.environment=prod"),
                ]),
            )
            .unwrap()
            .unwrap();
        assert_eq!(s.endpoint.url(), "http://collector:4318/v1/traces");
        assert_eq!(s.headers[0], ("dd-api-key".into(), "abc=".into()));
        assert_eq!(s.resource[0], ("service.name".into(), "orders-db".into()));
        assert!(s
            .resource
            .contains(&("deployment.environment".into(), "prod".into())));
        assert_eq!(s.sample, 1.0);
        let s = Options {
            endpoint: Some("http://127.0.0.1:4318".into()),
            ..Options::default()
        }
        .settings_from(
            "s",
            &env(&[("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "http://other/x")]),
        )
        .unwrap()
        .unwrap();
        assert_eq!(s.endpoint.url(), "http://127.0.0.1:4318/v1/traces");
        let err = Options {
            endpoint: Some("https://api.honeycomb.io".into()),
            ..Options::default()
        }
        .settings_from("s", &env(&[]))
        .unwrap_err();
        assert!(err.contains("no TLS"), "{err}");
        assert!(Options {
            endpoint: Some("http://x:4317".into()),
            ..Options::default()
        }
        .settings_from("s", &env(&[("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc")]))
        .is_err());
        assert!(Options::default()
            .settings_from(
                "s",
                &env(&[
                    ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://x:4318"),
                    ("OTEL_SDK_DISABLED", "true")
                ])
            )
            .unwrap()
            .is_none());
    }

    #[test]
    fn spans_are_encoded_as_otlp_json() {
        let t = Trace {
            id: [1; 16],
            state: Some("dd=s:1".into()),
            spans: vec![Rec {
                name: Cow::Borrowed("POST /query"),
                id: [2; 8],
                parent: Some([3; 8]),
                kind: Kind::Server,
                start: 10,
                end: 20,
                attrs: vec![
                    ("http.response.status_code", Val::Int(500)),
                    ("db.system", Val::from("fenecdb")),
                    ("x", Val::Bool(true)),
                ],
                error: Some("500".into()),
            }],
        };
        let mut out = String::new();
        encode(&mut out, &[("service.name".into(), "s".into())], &[t]);
        let v = fenec_core::json::parse_json(&out).unwrap();
        let text = format!("{v:?}");
        assert!(
            out.contains(r#""traceId":"01010101010101010101010101010101""#),
            "{out}"
        );
        assert!(
            out.contains(r#""parentSpanId":"0303030303030303""#),
            "{out}"
        );
        assert!(out.contains(r#""intValue":"500""#), "{out}");
        assert!(
            out.contains(r#""status":{"code":2,"message":"500"}"#),
            "{out}"
        );
        assert!(text.contains("fenecdb"));
    }
}
