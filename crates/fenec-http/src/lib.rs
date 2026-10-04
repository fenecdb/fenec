//! # fenec-http
//!
//! fenecdb's HTTP/JSON endpoint: a REST surface that needs no driver.
//! `fetch`, `curl` or any language's standard library is enough.
//!
//! ```text
//! curl 'http://127.0.0.1:8080/articles?year=gte.2024&tags=has.rust&limit=5'
//! curl -X POST http://127.0.0.1:8080/articles -d '{"title":"a","year":2024}'
//! curl -X POST http://127.0.0.1:8080/articles/near \
//!      -d '{"vector":[0.1,0.2],"limit":5,"where":"year >= 2024"}'
//! ```
//!
//! The server **shares** the database, it does not own it: it takes an
//! `Arc<RwLock<Database>>`. fenecdb is single-writer and two processes cannot
//! write to one file, so everything that writes a file -- this endpoint, a
//! replica's follower, the graph keeper, `--follow`'s mirror -- is a thread
//! of one process, `fenec-server`, which keeps the sync policy, the
//! checkpoint and the memory ceiling in one place.
//!
//! There is no TLS: a non-loopback address wants a token, and a TLS
//! terminator is needed in front of it on an open network.
//!
//! With [`Server::with_tenants`] one listener serves many databases, one
//! file each, under `/t/<tenant>/...` -- the same surface as a single file
//! below the prefix, so a client's base URL is the only thing that changes.
//! See [`tenants`] for why a file per tenant.

/// Writes a line to stderr and never panics. `eprintln!` panics when stderr
/// is gone -- a closed pipe, a log collector that restarted -- and a server
/// thread that dies that way while logging a sync error, or while shutting
/// down, leaves the process running and deaf to SIGTERM: the thread that
/// would have acted on the signal is the one that died.
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {{
        use ::std::io::Write as _;
        let _ = ::std::writeln!(::std::io::stderr(), $($arg)*);
    }};
}

pub mod access;
pub mod admin;
pub mod api;
pub mod archive;
pub mod audit;
pub mod cdc;
pub mod crypto;
pub mod held;
pub mod http;
pub mod idempotent;
pub mod lease;
pub mod link;
pub mod metrics;
pub mod replication;
pub mod seal;
pub mod sse;
pub mod statements;
pub mod sweep;
pub mod tenants;
pub mod timing;

use access::Who;
use fenec_core::prelude::*;
use http::{Method, Request, Response};
use replication::Replication;
use sse::Hub;
use std::io::BufReader;
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tenants::{Refused, Tenants};

/// Stack of a connection's thread. `thread::spawn`'s default is 2 MiB; the
/// deepest accepted expression (`fenec_ql::MAX_EXPR_DEPTH` = 512) needs
/// ~750 KiB in release and ~5 MiB in a debug build. So the default is tight
/// in the first case (2.7x) and short in the second -- and a stack overflow
/// is not a catchable panic but an `abort` of the process: a single deep
/// query would take the whole server down. 8 MiB is *virtual* space;
/// untouched pages are never resident (measured: RSS 5.2 MB over 100 idle
/// connections, ~36 KiB a connection, whatever the stack).
const CONNECTION_STACK: usize = 8 << 20;

#[derive(Clone)]
pub struct Config {
    pub addr: String,
    /// When set, every request requires `Authorization: Bearer <token>`.
    pub token: Option<String>,
    /// The `Access-Control-Allow-Origin` value (`*` or a single origin).
    /// With `None` no CORS header is sent at all -- non-browser clients are
    /// unaffected.
    pub cors: Option<String>,
    /// Turns off the write endpoints: for a read API open to the public net.
    pub read_only: bool,
    /// Since there is no TLS, binding outside loopback without auth is refused.
    pub insecure: bool,
    /// Ceiling on concurrent connections (0 = unlimited). Every connection is
    /// a thread.
    pub max_connections: usize,
    /// Request body ceiling. Bulk `put` bodies can be large, but leaving it
    /// uncapped hands memory to the client.
    pub max_body: usize,
    /// Silence ceiling while waiting for the next request on a keep-alive connection.
    pub idle_timeout: Option<Duration>,
    /// `sync` after every write (the equivalent of fenec-server's `--sync always`).
    pub sync_on_write: bool,
    /// How long a write's `Idempotency-Key` and answer are kept.
    pub idempotency_ttl: Duration,
    /// Ceiling on concurrent **subscriptions** (0 = unlimited).
    ///
    /// Counted apart from ordinary requests. Subscriptions are long lived and
    /// each holds a thread; sharing a single ceiling would let
    /// `max_connections` subscribers close the server to ordinary requests.
    pub max_streams: usize,
    /// Interval of the keep-alive line when nothing happens on a subscription.
    /// Proxies and NAT tables drop silent connections.
    pub stream_keepalive: Duration,
    /// Stall ceiling while writing to a subscriber. A client that does not
    /// read must not hold a thread forever.
    pub stream_write_timeout: Duration,
    /// Entry count of the change ring: how far behind a subscriber may fall.
    /// On overflow the subscriber is reseeded.
    pub change_capacity: usize,
    /// Token for `/_admin/` (tenant mode only). Without it the admin
    /// endpoints are off.
    pub admin_token: Option<String>,
    /// Body ceiling for `PUT /_admin/tenants/<t>/file`: an image is the
    /// whole tenant, far past what a data request needs.
    pub max_import: usize,
    /// JSON Web Tokens and the policy they are held to; see [`access`].
    pub access: Option<Arc<access::Access>>,
    /// Data footprint ceiling in bytes (0 = off): fenec-server's
    /// `--max-memory`, held on every write; see [`over_ceiling`].
    pub max_memory: usize,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            addr: "127.0.0.1:8080".into(),
            token: None,
            cors: None,
            read_only: false,
            insecure: false,
            max_connections: 100,
            max_body: 64 << 20,
            idle_timeout: Some(Duration::from_secs(60)),
            sync_on_write: false,
            idempotency_ttl: Duration::from_secs(24 * 3600),
            max_streams: 64,
            stream_keepalive: Duration::from_secs(20),
            stream_write_timeout: Duration::from_secs(30),
            change_capacity: fenec_core::changes::DEFAULT_CAPACITY,
            admin_token: None,
            max_import: 1 << 30,
            access: None,
            max_memory: 0,
        }
    }
}

pub struct Server {
    backend: Backend,
    cfg: Arc<Config>,
    live: Arc<AtomicUsize>,
}

#[derive(Clone)]
enum Backend {
    Single {
        db: Arc<RwLock<Database>>,
        hub: Arc<Hub>,
        /// Feeding replicas, following a primary, or both; see
        /// [`replication`].
        repl: Option<Arc<Replication>>,
    },
    Tenants(Arc<Tenants>),
    /// `--metrics <address>`: `/_metrics` and nothing else, for a server
    /// whose data is served on another address.
    Metrics {
        db: Arc<RwLock<Database>>,
        repl: Option<Arc<Replication>>,
    },
}

impl Server {
    /// Builds the server and attaches itself to the database as a **watcher**:
    /// from then on every write -- whether it comes from HTTP or from
    /// `fenec-server` -- wakes the waiting subscriptions. One process, one writer,
    /// one wake-up point.
    pub fn new(db: Arc<RwLock<Database>>, cfg: Config) -> Server {
        let hub = Hub::new();
        {
            let mut guard = db.write().unwrap_or_else(|e| e.into_inner());
            guard.set_watcher(Arc::clone(&hub) as Arc<dyn Watcher>);
            guard.set_change_capacity(cfg.change_capacity);
            if cfg.access.is_some() {
                // Once per database: a second server over it finds it there.
                let _ = guard.install_plugin(&access::CheckPlugin);
            }
        }
        Server {
            backend: Backend::Single {
                db,
                hub,
                repl: None,
            },
            cfg: Arc::new(cfg),
            live: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Serves `/_replication` for a single database: the stream replicas
    /// are fed from, the status, and a replica's promotion. That path is
    /// then this server's -- a collection named `_replication` is not
    /// reachable over HTTP while it is.
    pub fn with_replication(mut self, repl: Arc<Replication>) -> Server {
        if let Backend::Single { repl: slot, .. } = &mut self.backend {
            *slot = Some(repl);
        }
        self
    }

    /// One database per tenant under `/t/<tenant>/`, plus `/_admin/`. Each
    /// tenant gets its own watcher as it is opened.
    pub fn with_tenants(tenants: Arc<Tenants>, cfg: Config) -> Server {
        if cfg.access.is_some() {
            tenants.check_scoped_writes();
        }
        Server {
            backend: Backend::Tenants(tenants),
            cfg: Arc::new(cfg),
            live: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// A listener that answers `/_metrics` and nothing else. Unlike
    /// [`Server::new`] it leaves the database's watcher alone: a second
    /// watcher would take the wake-ups from the subscriptions of the HTTP
    /// endpoint serving the same database.
    pub fn metrics_only(
        db: Arc<RwLock<Database>>,
        repl: Option<Arc<Replication>>,
        cfg: Config,
    ) -> Server {
        Server {
            backend: Backend::Metrics { db, repl },
            cfg: Arc::new(cfg),
            live: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Number of live subscriptions (for measurement and tests). Single
    /// database only; a tenant's streams are counted on its own hub.
    pub fn live_streams(&self) -> usize {
        match &self.backend {
            Backend::Single { hub, .. } => hub.live(),
            Backend::Tenants(_) | Backend::Metrics { .. } => 0,
        }
    }

    /// Opens the listener. A non-loopback address is not accepted without a
    /// token unless `--insecure` is given.
    pub fn bind(&self) -> std::io::Result<TcpListener> {
        let guarded = self.cfg.token.is_some()
            || self.cfg.access.is_some()
            || matches!(self.backend, Backend::Metrics { .. }) && self.cfg.admin_token.is_some();
        if is_remote(&self.cfg.addr) && !guarded && !self.cfg.insecure {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "{} is not a loopback address and there is no token.\n\
                     The HTTP endpoint does not speak TLS; listening without a\n\
                     token on an open network exposes the whole database to\n\
                     everyone. Pass `--http-token` (or --insecure if deliberate).",
                    self.cfg.addr
                ),
            ));
        }
        TcpListener::bind(&self.cfg.addr)
    }

    pub fn serve(&self) -> std::io::Result<()> {
        let listener = self.bind()?;
        self.serve_on(listener)
    }

    pub fn serve_on(&self, listener: TcpListener) -> std::io::Result<()> {
        metrics::started();
        let auth = if self.cfg.token.is_some() || self.cfg.admin_token.is_some() {
            "token"
        } else {
            "no auth"
        };
        if matches!(self.backend, Backend::Metrics { .. }) {
            crate::log!(
                "metrics on: http://{}/_metrics  [{auth}]",
                listener.local_addr()?
            );
        } else {
            crate::log!(
                "fenec-http {} listening on: http://{}  [{}{}]",
                fenec_core::VERSION,
                listener.local_addr()?,
                if self.cfg.token.is_some() {
                    "token"
                } else {
                    "no auth"
                },
                if self.cfg.read_only {
                    ", read only"
                } else {
                    ""
                },
            );
        }

        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
            if self.cfg.max_connections > 0 && live > self.cfg.max_connections {
                self.live.fetch_sub(1, Ordering::SeqCst);
                let mut s = stream;
                let _ = Response::error(503, "too many connections").write(&mut s, false, false);
                continue;
            }

            let backend = self.backend.clone();
            let cfg = Arc::clone(&self.cfg);
            let counter = Arc::clone(&self.live);
            let spawned = std::thread::Builder::new()
                .name("fenec-http".into())
                .stack_size(CONNECTION_STACK)
                .spawn(move || {
                    serve_connection(stream, &backend, &cfg);
                    counter.fetch_sub(1, Ordering::SeqCst);
                });
            if spawned.is_err() {
                self.live.fetch_sub(1, Ordering::SeqCst);
            }
        }
        Ok(())
    }
}

/// Who a request is: the server's token is everything, a JSON Web Token
/// what its rules allow, and with neither configured, anyone is everything.
/// An `Err` is the refusal to send.
///
/// It stands apart from `handle` because of the subscription path: that path
/// produces no response and takes the connection over -- but it cannot skip
/// authentication.
fn authenticate(cfg: &Config, req: &Request) -> std::result::Result<Who, Response> {
    let given = req
        .header("authorization")
        .and_then(|v| v.strip_prefix("Bearer "));
    let refuse = |why: &str| Response::error(401, why).header("WWW-Authenticate", "Bearer");
    if let (Some(token), Some(given)) = (&cfg.token, given) {
        if constant_eq(given.as_bytes(), token.as_bytes()) {
            return Ok(Who::Full);
        }
    }
    if let (Some(access), Some(given)) = (&cfg.access, given) {
        if given.matches('.').count() == 2 {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            return match access.scope(given, now) {
                Ok(scope) => Ok(Who::Scoped(Arc::new(scope))),
                Err(why) => Err(refuse(why)),
            };
        }
    }
    if cfg.token.is_none() && cfg.access.is_none() {
        return Ok(Who::Full);
    }
    Err(refuse("invalid or missing token"))
}

/// Whether `req` may read what every statement cost where it is sent: what
/// reads the data there without a scope, or the admin token. A JWT's user
/// is held to its rows, and the statements name every collection.
fn full(cfg: &Config, req: &Request) -> bool {
    matches!(authenticate(cfg, req), Ok(Who::Full)) || metrics::allowed(cfg, req, true)
}

/// The statement as `who` may run it.
fn scoped(who: &Who, stmt: Statement) -> fenec_core::error::Result<Statement> {
    match who.scope() {
        None => Ok(stmt),
        Some(scope) => scope.rewrite(stmt),
    }
}

/// A list of collections, cut to those `who` may read.
fn visible(who: &Who, resp: Response2) -> Response2 {
    match (who.scope(), resp) {
        (Some(scope), fenec_core::query::Response::Schemas(mut list)) => {
            list.retain(|s| scope.readable(&s.name));
            fenec_core::query::Response::Schemas(list)
        }
        (_, resp) => resp,
    }
}

type Response2 = fenec_core::query::Response;

fn is_remote(addr: &str) -> bool {
    match addr.to_socket_addrs() {
        Ok(mut it) => it.any(|a| !a.ip().is_loopback()),
        Err(_) => false,
    }
}

/// A single connection: reads consecutive requests on a keep-alive connection.
fn serve_connection(stream: TcpStream, backend: &Backend, cfg: &Config) {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(cfg.idle_timeout);
    let peer = stream.peer_addr().ok();
    audit::connection("http", peer);
    let Ok(write_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    let mut out = write_half;
    // After the socket, so it is dropped first: a client that saw the
    // connection close finds it no longer counted. A scrape is not one of
    // the database's clients.
    let _open = (!matches!(backend, Backend::Metrics { .. })).then(metrics::Connection::open);

    // The body is read before the path is looked at, so in tenant mode the
    // reading ceiling is the larger of the two and a data request over
    // `max_body` is refused afterwards.
    let tenant_mode = matches!(backend, Backend::Tenants(_));
    let ceiling = if tenant_mode && cfg.admin_token.is_some() {
        cfg.max_body.max(cfg.max_import)
    } else {
        cfg.max_body
    };

    loop {
        // With the `timing` feature the clock starts once the request's
        // first bytes are in, not while the connection waits for them.
        #[cfg(feature = "timing")]
        {
            use std::io::BufRead as _;
            let _ = reader.fill_buf();
            timing::begin();
        }
        let mut req = match http::read_request(&mut reader, ceiling) {
            Ok(Some(req)) => req,
            Ok(None) => return,
            Err(http::BadRequest(status, msg)) => {
                let _ = cors(Response::error(status, &msg), cfg).write(&mut out, false, false);
                return;
            }
        };
        let keep_alive = req.keep_alive;
        let head_only = req.method == Method::Head;
        timing::lap(timing::Phase::Http);
        #[cfg(feature = "timing")]
        if req.segments() == ["_timing"] {
            timing::end();
            if timing::handle(&req)
                .write(&mut out, keep_alive, head_only)
                .is_err()
                || !keep_alive
            {
                return;
            }
            continue;
        }

        // `fenec-server --ping` and a container's health check: answered
        // with no token and no lock, before any routing. A probe that ran a
        // query would wait out a long `compact` for the lock, and a healthy
        // server would look dead; one that needed a token would put the
        // token in every orchestrator's configuration.
        if matches!(req.method, Method::Get | Method::Head)
            && req.segments() == ["_health"]
            && !matches!(backend, Backend::Metrics { .. })
        {
            let resp = Response::json(200, &br#"{"ok":true}"#[..]);
            if resp.write(&mut out, keep_alive, head_only).is_err() || !keep_alive {
                return;
            }
            continue;
        }

        // Before any routing: the metrics are the node's, not a tenant's.
        let scrape =
            matches!(req.method, Method::Get | Method::Head) && req.segments() == ["_metrics"];
        if scrape || matches!(backend, Backend::Metrics { .. }) {
            let resp = match backend {
                _ if !scrape => Response::error(404, "this listener serves /_metrics alone"),
                Backend::Single { db, repl, .. } | Backend::Metrics { db, repl } => {
                    let repl = repl.as_deref();
                    metrics::handle(cfg, &req, metrics::Source::Single { db, repl })
                }
                Backend::Tenants(t) => {
                    metrics::handle(cfg, &req, metrics::Source::Tenants(t.stats()))
                }
            };
            audit::http(&req, resp.status, peer);
            if resp.write(&mut out, keep_alive, head_only).is_err() || !keep_alive {
                return;
            }
            continue;
        }

        // The node's own statements -- on a tenant node every tenant's, its
        // admin's alone. A tenant's own are under its prefix, routed below.
        if req.segments() == ["_stats", "statements"] {
            let (view, allowed) = match backend {
                Backend::Tenants(_) => {
                    (statements::View::Tenants, metrics::allowed(cfg, &req, true))
                }
                _ => (statements::View::Node, full(cfg, &req)),
            };
            let resp = cors(statements::handle(&req, view, allowed), cfg);
            audit::http(&req, resp.status, peer);
            if resp.write(&mut out, keep_alive, head_only).is_err() || !keep_alive {
                return;
            }
            continue;
        }

        // The database is only ever borrowed from the tenant, never cloned
        // out of it: the tenant's `Arc` count is what says "in use", and a
        // clone of the inner `Arc` would keep the database alive past a
        // close without the registry knowing.
        let tenant = match backend {
            Backend::Single { .. } | Backend::Metrics { .. } => None,
            Backend::Tenants(tenants) => match route_tenant(tenants, cfg, &mut req) {
                Ok(t) => Some(t),
                Err(resp) => {
                    audit::http(&req, resp.status, peer);
                    let resp = cors(resp, cfg);
                    if resp.write(&mut out, keep_alive, head_only).is_err() || !keep_alive {
                        return;
                    }
                    continue;
                }
            },
        };
        if let (Some(t), ["_stats", "statements"]) = (&tenant, req.segments().as_slice()) {
            let view = statements::View::Tenant(t.name());
            let resp = cors(statements::handle(&req, view, full(cfg, &req)), cfg);
            audit::http(&req, resp.status, peer);
            if resp.write(&mut out, keep_alive, head_only).is_err() || !keep_alive {
                return;
            }
            continue;
        }
        let (db, hub) = match (&tenant, backend) {
            (Some(t), _) => (&t.db, &t.hub),
            (None, Backend::Single { db, hub, .. }) => (db, hub),
            (None, Backend::Tenants(_) | Backend::Metrics { .. }) => unreachable!("routed above"),
        };
        // A tenant's own replication, where the node replicates its tenants:
        // `/t/<tenant>/_replication` is the tenant's `/_replication`, and the
        // path was stripped above.
        let repl = match (&tenant, backend) {
            (Some(t), _) => t.repl.as_ref(),
            (None, Backend::Single { repl, .. }) => repl.as_ref(),
            (None, _) => None,
        };
        if let Some(repl) = repl {
            if req.segments().first() == Some(&"_replication") {
                // A replica's stream is a body with no end, like a
                // subscription: it takes the connection over.
                let _ = out.set_read_timeout(None);
                match replication::handle(&mut out, db, repl, &req) {
                    None => return,
                    Some(resp) => {
                        audit::http(&req, resp.status, peer);
                        if cors(resp, cfg)
                            .write(&mut out, keep_alive, head_only)
                            .is_err()
                            || !keep_alive
                        {
                            return;
                        }
                        let _ = out.set_read_timeout(cfg.idle_timeout);
                        continue;
                    }
                }
            }
        }
        // The writes on disk, documents and all (`cdc.rs`): to what reads
        // every row. A scoped token's filter holds no deletion of a row it
        // never saw, as a scoped subscription's ids do, so it is refused.
        if req.segments().first() == Some(&"_changes") {
            let resp = match (authenticate(cfg, &req), repl.and_then(|r| r.feed())) {
                (Err(deny), _) => deny,
                (Ok(Who::Scoped(_)), _) => {
                    Response::error(403, "a scoped token cannot read the change stream")
                }
                (Ok(_), None) => Response::error(
                    409,
                    "this server keeps no change feed: start it with --cdc or --replication-token",
                ),
                (Ok(_), Some(feed)) => cdc::route(db, feed, cfg, &req),
            };
            audit::http(&req, resp.status, peer);
            if cors(resp, cfg)
                .write(&mut out, keep_alive, head_only)
                .is_err()
                || !keep_alive
            {
                return;
            }
            continue;
        }
        // A subscription cannot go down the ordinary response path: it is a
        // body with unknown `Content-Length` and no end. It takes the
        // connection over and never returns.
        if is_stream(&req) {
            let who = match authenticate(cfg, &req) {
                Ok(who) => who,
                Err(deny) => {
                    audit::http(&req, deny.status, peer);
                    let _ = cors(deny, cfg).write(&mut out, false, false);
                    return;
                }
            };
            // A read timeout is meaningless during the stream: the client
            // sends nothing, and the wait is on a Condvar.
            let _ = out.set_read_timeout(None);
            sse::serve(&mut out, db, cfg, hub, &req, &who);
            return;
        }
        // Reading one's own write on a replica: the request waits until the
        // write its client made on the primary (`Fenec-Seq`) is here.
        if let Some(refusal) = after(&req, db, hub) {
            if cors(refusal, cfg)
                .write(&mut out, keep_alive, head_only)
                .is_err()
                || !keep_alive
            {
                return;
            }
            continue;
        }
        timing::lap(timing::Phase::Route);
        let started = std::time::Instant::now();
        let resp = match &tenant {
            None => handle(db, cfg, &req),
            Some(t) => handle_tenant(t, cfg, &req),
        };
        // A preflight is the browser's, not a statement.
        if req.method != Method::Options {
            let (took, failed) = (started.elapsed(), resp.status >= 400);
            let what = describe(&req);
            metrics::record(took, failed, || what.clone());
            statements::record(tenant.as_ref().map(|t| t.name()), &what, took, failed);
        }
        // Let go of the tenant before writing: a slow client must not keep
        // it from closing.
        drop(tenant);
        audit::http(&req, resp.status, peer);
        let resp = cors(resp, cfg);
        timing::lap(timing::Phase::Books);
        if resp.write(&mut out, keep_alive, head_only).is_err() || !keep_alive {
            return;
        }
        timing::lap(timing::Phase::Write);
        timing::end();
    }
}

/// How long a request sent with `Fenec-After` waits for the write it names,
/// unless `Fenec-Wait` says otherwise, and the longest it may.
const AFTER_WAIT: Duration = Duration::from_secs(5);
const AFTER_LONGEST: Duration = Duration::from_secs(30);

/// A request sent with `Fenec-After: <n>` is served once this database holds
/// change `n` -- a replica, once it has applied the write its client made on
/// the primary and was answered with `Fenec-Seq: <n>` -- or answered 504
/// after the wait, never with what came before it.
fn after(req: &Request, db: &RwLock<Database>, hub: &sse::Hub) -> Option<Response> {
    let n = req.header("fenec-after")?;
    let Ok(n) = n.trim().parse::<u64>() else {
        return Some(Response::error(400, "Fenec-After expects a change number"));
    };
    let wait = req
        .header("fenec-wait")
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(AFTER_WAIT, Duration::from_millis)
        .min(AFTER_LONGEST);
    let seq = || db.read().unwrap_or_else(|e| e.into_inner()).change_seq();
    match hub.reached(n, seq, std::time::Instant::now() + wait) {
        true => None,
        false => Some(Response::json(
            504,
            format!(
                "{{\"error\":\"change {n} has not reached this server in the wait\",\"seq\":{}}}",
                seq()
            ),
        )),
    }
}

/// A write's answer, with the change it left the database at: a replica
/// sent `Fenec-After` with it serves the read once it holds the write.
fn with_seq(mut resp: Response, seq: Option<u64>) -> Response {
    if resp.status < 300 {
        resp.seq = seq;
    }
    resp
}

/// What the slow-statement log says a request was: its method and target,
/// and for raw FenecQL the statements, which the target does not hold.
fn describe(req: &Request) -> String {
    let mut s = format!("{} {}", req.method.name(), req.target);
    if matches!(req.segments().as_slice(), ["query"] | ["batch"]) {
        s.push(' ');
        s.push_str(&String::from_utf8_lossy(&req.body));
    }
    s
}

/// Resolves `/t/<tenant>/rest` and strips the prefix, so everything below
/// sees the single-database surface. `/_admin/` is answered here, and so is
/// every refusal; `Ok` means "serve this request against the tenant".
fn route_tenant(
    tenants: &Tenants,
    cfg: &Config,
    req: &mut Request,
) -> std::result::Result<Arc<tenants::Tenant>, Response> {
    let segs = req.segments();
    if segs.first() == Some(&"_admin") {
        return Err(admin::handle(tenants, cfg, req));
    }
    if req.body.len() > cfg.max_body {
        return Err(Response::error(
            413,
            &format!(
                "the body is {} bytes, the ceiling is {}",
                req.body.len(),
                cfg.max_body
            ),
        ));
    }
    let name = match segs.as_slice() {
        ["t", name, ..] => name.to_string(),
        _ => {
            return Err(Response::error(
                404,
                "this node serves tenants: /t/<tenant>/...",
            ))
        }
    };
    // The token is checked before the tenant is looked up: otherwise a 404
    // against a 401 would tell an unauthenticated caller which tenants exist.
    // A tenant's replication stream carries the node's replication token
    // rather than the data one -- a replica is not a client -- so that is
    // the one asked for there.
    if segs.get(2) == Some(&"_replication") {
        let given = req
            .header("authorization")
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");
        let ok = tenants
            .replication_token()
            .is_some_and(|t| constant_eq(given.as_bytes(), t.as_bytes()));
        if !ok {
            return Err(Response::error(401, "invalid or missing replication token")
                .header("WWW-Authenticate", "Bearer"));
        }
    } else if let Who::Scoped(scope) = authenticate(cfg, req)? {
        // A token is held to the tenant it names here, before the tenant
        // is looked up and once for every route below the prefix: the
        // statements, REST, `/batch`, subscriptions, `/_changes`,
        // `/_schema`, the statements' counts. The node's token and an
        // unscoped server are unaffected.
        scope
            .reaches(&name)
            .map_err(|why| Response::error(403, why))?;
    }
    let t = tenants
        .get(&name)
        .map_err(|Refused(status, msg)| Response::error(status, &msg))?;
    // Rebuilt from the segments rather than cut at an offset: `//t/acme`
    // has the same segments as `/t/acme` but not the same prefix length.
    req.path = format!("/{}", segs[2..].join("/"));
    Ok(t)
}

/// A tenant request: held against `freeze` for its whole handling. A frozen
/// tenant, or one this node's lease does not let it write, answers as a
/// read-only server would, then the refusal is turned into 503 with
/// `Retry-After` -- a move or a failover is in progress, and the right thing
/// for the client is to come back, through the router, not to give up.
///
/// A write let in may still be refused by the engine's fence
/// (`Database::set_fence`) if the lease lapses between the two checks, so
/// the node is asked again once a 403 comes back: a put let in just before
/// the lease ran out was answered 403 on a slow CI runner, as if the write
/// were not this server's to take, when the client should come back.
fn handle_tenant(t: &tenants::Tenant, cfg: &Config, req: &Request) -> Response {
    let _held = t.enter();
    if cfg.read_only {
        return handle(&t.db, cfg, req);
    }
    let refused = refusal(t);
    let resp = match refused {
        None => handle(&t.db, cfg, req),
        Some(_) => {
            let read_only = Config {
                read_only: true,
                ..cfg.clone()
            };
            handle(&t.db, &read_only, req)
        }
    };
    match refused.or_else(|| refusal(t)) {
        Some(why) if resp.status == 403 => Response::error(503, &why).header("Retry-After", "1"),
        _ => resp,
    }
}

/// Why this node takes no write for `t` now, if it takes none.
fn refusal(t: &tenants::Tenant) -> Option<String> {
    match t.is_frozen() {
        true => Some("the tenant is being moved; retry shortly".to_string()),
        false => t.writable().err(),
    }
}

/// `GET /<name>/changes`
fn is_stream(req: &Request) -> bool {
    req.method == Method::Get && matches!(req.segments().as_slice(), [_, "changes"])
}

/// Turns a request into a response: auth, preflight, routing, execution.
pub fn handle(db: &Arc<RwLock<Database>>, cfg: &Config, req: &Request) -> Response {
    let who = match authenticate(cfg, req) {
        Ok(who) => who,
        Err(deny) => return deny,
    };

    // A preflight request never reaches the database.
    if req.method == Method::Options {
        return Response::empty(204);
    }

    // Raw FenecQL input: its shape is only known after parsing, so it makes
    // its own locking decision.
    if req.method == Method::Post && req.segments() == ["query"] {
        return handle_query(db, cfg, req, &who);
    }

    // Batch: several statements, one lock, one round trip.
    if req.method == Method::Post && req.segments() == ["batch"] {
        return handle_batch(db, cfg, req, &who);
    }

    // A schema declared in code (`fenec_core::declared`).
    if req.segments().first() == Some(&"_schema") {
        return handle_schema(db, cfg, req, &who);
    }

    // Read-only mode rejects before routing: whichever collection it is, the
    // answer is 403.
    if cfg.read_only && wants_write(req) {
        return Response::error(403, "the server is in read-only mode");
    }

    // The write path does its routing under the write lock too: if the lock
    // is released between the schema lookup and execution, a `drop
    // collection` arriving in between leaves the two stages inconsistent.
    if wants_write(req) {
        metrics::wrote();
        let key = match idempotent::key(req, &who) {
            Ok(k) => k,
            Err(refusal) => return refusal,
        };
        let mut guard = held::write(db);
        let ttl = cfg.idempotency_ttl.as_millis() as i64;
        if let Some(sent) = key
            .as_ref()
            .and_then(|k| idempotent::answered(&guard, k, ttl))
        {
            return sent;
        }
        let routed = match api::route(&guard, req) {
            Ok(r) => r,
            Err(e) => return error_response(&e),
        };
        let stmt = match scoped(&who, routed.statement) {
            Ok(s) => s,
            Err(e) => return error_response(&e),
        };
        if let Some(why) = over_ceiling(cfg.max_memory, &guard, &stmt) {
            return refused(why);
        }
        if key.is_some() {
            if let Err(e) = guard.begin() {
                return error_response(&e);
            }
        }
        let result = access::within(&who, || guard.execute_with(&stmt, &[]));
        let answer =
            |result: &fenec_core::error::Result<fenec_core::prelude::Response>| match result {
                Ok(resp) => {
                    statements::rows(counted(resp));
                    api::render(resp, &routed.shape, fenec_core::VERSION)
                }
                Err(e) => error_response(e),
            };
        // A key keeps the answer, so it is made under the lock; without
        // one, after it, as before.
        let kept = match &key {
            Some(k) => {
                let resp = answer(&result);
                if let Err(e) = keyed(&mut guard, k, &resp, result.is_ok(), ttl) {
                    return error_response(&e);
                }
                Some(resp)
            }
            None => None,
        };
        let seq = result.is_ok().then(|| guard.change_seq());
        let durability = match result {
            Ok(_) => match flush_for(cfg, &mut guard) {
                Ok(d) => d,
                Err(e) => return error_response(&e),
            },
            Err(_) => None,
        };
        drop(guard);
        if let Err(e) = await_durable(db, durability) {
            return error_response(&e);
        }
        with_seq(kept.unwrap_or_else(|| answer(&result)), seq)
    } else {
        let guard = held::read(db);
        let routed = match api::route(&guard, req) {
            Ok(r) => r,
            Err(e) => return error_response(&e),
        };
        let stmt = match scoped(&who, routed.statement) {
            Ok(s) => s,
            Err(e) => return error_response(&e),
        };
        match guard.query(&stmt, &[]) {
            Ok(resp) => {
                let resp = visible(&who, resp);
                statements::rows(counted(&resp));
                api::render(&resp, &routed.shape, fenec_core::VERSION)
            }
            Err(e) => error_response(&e),
        }
    }
}

/// `/_schema`: a schema declared in code -- the description every SDK's
/// declarations compile to -- against the database.
///
/// ```text
/// GET  /_schema                      the database's schema as a description
/// POST /_schema/plan                 what an apply would do; writes nothing
/// POST /_schema/plan?mode=follow     what a client that does not own it lacks
/// POST /_schema/apply                migrations, then what only adds: one block
/// ```
///
/// The server owns its schema, so a client whose code declares one -- a
/// synced replica, an app over HTTP -- compares by default (`follow`:
/// everything it declares must be here as declared, and what is here beside
/// it is the server's). Applying is for the code that owns the database, a
/// deploy step running its migrations as drizzle-kit's `migrate` does: it
/// takes what reads and writes everything, the server's token, never a
/// scoped one. The plan and the apply are made under the write lock, so
/// nothing changes the schema between them.
fn handle_schema(db: &Arc<RwLock<Database>>, cfg: &Config, req: &Request, who: &Who) -> Response {
    let body = std::str::from_utf8(&req.body).unwrap_or("");
    let follow = req.query.iter().any(|(k, v)| k == "mode" && v == "follow");
    let full = matches!(who, Who::Full);
    let visible = |db: &Database| -> Vec<fenec_core::schema::Schema> {
        db.collection_names()
            .iter()
            .filter(|n| who.scope().is_none_or(|s| s.readable(n)))
            .filter_map(|n| db.collection(n).ok().map(|c| c.schema.clone()))
            .collect()
    };
    let outcome = match (req.method, req.segments().as_slice()) {
        (Method::Get | Method::Head, ["_schema"]) => {
            let schemas = visible(&held::read(db));
            return Response::json(200, fenec_core::declared::describe(&schemas));
        }
        (Method::Post, ["_schema", "plan"]) if follow => {
            // A scoped token compares what it may read: a collection it may
            // not is one the server does not have, for it.
            let schemas = visible(&held::read(db));
            let schemas: Vec<_> = schemas.iter().collect();
            fenec_abi::read(body).map(|d| fenec_abi::Outcome {
                plan: fenec_core::declared::plan(
                    &schemas,
                    &d.collections,
                    fenec_core::declared::Mode::Follow,
                ),
                ..Default::default()
            })
        }
        (Method::Post, ["_schema", "plan" | "apply"]) if !full => return Response::error(
            403,
            "the schema is the server's: a scoped token compares (?mode=follow) and does not apply",
        ),
        (Method::Post, ["_schema", "plan"]) => {
            fenec_abi::schema(&mut held::write(db), body, false, None)
        }
        (Method::Post, ["_schema", "apply"]) => {
            if cfg.read_only {
                return Response::error(403, "the server is in read-only mode");
            }
            metrics::wrote();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as i64);
            let mut guard = held::write(db);
            let r = fenec_abi::schema(&mut guard, body, true, Some(now));
            let durability = match &r {
                Ok(o) if o.applied => match flush_for(cfg, &mut guard) {
                    Ok(d) => d,
                    Err(e) => return error_response(&e),
                },
                _ => None,
            };
            let seq = guard.change_seq();
            drop(guard);
            if let Err(e) = await_durable(db, durability) {
                return error_response(&e);
            }
            return match r {
                Ok(o) if o.plan.refusals.is_empty() => {
                    with_seq(Response::json(200, o.json()), Some(seq))
                }
                Ok(o) => Response::json(409, o.json()),
                Err(e) => error_response(&e),
            };
        }
        _ => {
            return Response::error(
                404,
                "the schema is GET /_schema, POST /_schema/plan or /_schema/apply",
            )
        }
    };
    match outcome {
        Ok(o) => Response::json(200, o.json()),
        Err(e) => error_response(&e),
    }
}

/// Lands the block a keyed write ran in with its key and answer, or puts it
/// back when the write failed: a failed write keeps no key, and is made
/// when sent again.
fn keyed(
    db: &mut Database,
    key: &idempotent::Key,
    resp: &Response,
    ok: bool,
    ttl: i64,
) -> fenec_core::error::Result<()> {
    if !ok {
        db.rollback();
        return Ok(());
    }
    if let Err(e) = idempotent::keep(db, key, resp, ttl).and_then(|_| db.commit()) {
        db.rollback();
        return Err(e);
    }
    Ok(())
}

/// Raw FenecQL: parsed first (without a lock), then whether it reads or writes
/// is read off the statement itself.
fn handle_query(db: &Arc<RwLock<Database>>, cfg: &Config, req: &Request, who: &Who) -> Response {
    let body = match std::str::from_utf8(&req.body) {
        Ok(b) => b,
        Err(_) => return Response::error(400, "the body is not UTF-8"),
    };
    let (stmt, params) = match api::parse_query(body).and_then(|(stmt, params)| {
        let exact = api::exactly(db, body, stmt, params);
        timing::lap(timing::Phase::Exactly);
        exact
    }) {
        Ok(v) => v,
        Err(e) => return error_response(&e),
    };
    // A read is the same sent twice, and keeps no key.
    let key = match stmt.is_read_only() {
        true => None,
        false => match idempotent::key(req, who) {
            Ok(k) => k,
            Err(refusal) => return refusal,
        },
    };
    if key.is_some() && !stmt.fits_block() {
        return Response::error(
            400,
            "a compact cannot be put back, and takes no Idempotency-Key",
        );
    }
    // The statement parsed is shared (`api::parse_query`): a scope is ANDed
    // into a copy of it.
    let stmt = match who.scope() {
        None => stmt,
        Some(_) => match scoped(who, Arc::unwrap_or_clone(stmt)) {
            Ok(s) => Arc::new(s),
            Err(e) => return error_response(&e),
        },
    };
    if cfg.read_only && !stmt.is_read_only() {
        return Response::error(403, "the server is in read-only mode");
    }
    if !stmt.is_read_only() {
        metrics::wrote();
    }

    if !stmt.is_read_only() {
        let guard = db.read().unwrap_or_else(|e| e.into_inner());
        if let Some(why) = over_ceiling(cfg.max_memory, &guard, &stmt) {
            return refused(why);
        }
    }
    if let Some(k) = &key {
        return keyed_query(db, cfg, who, k, &stmt, &params);
    }
    // The change a write left the database at, for `Fenec-Seq`.
    let mut seq = None;
    let result = if stmt.is_read_only() {
        let r = held::read(db).query(&stmt, &params);
        timing::lap(timing::Phase::Execute);
        r
    } else if let Some(built) = Database::maintain(db, &stmt) {
        // `create index` and `compact` are built beside the database, with
        // no lock held; the index's record then waits for the disk as any
        // write does.
        let durability = match built {
            Ok(_) => {
                let mut g = db.write().unwrap_or_else(|e| e.into_inner());
                seq = Some(g.change_seq());
                match flush_for(cfg, &mut g) {
                    Ok(d) => d,
                    Err(e) => return error_response(&e),
                }
            }
            Err(_) => None,
        };
        if let Err(e) = await_durable(db, durability) {
            return error_response(&e);
        }
        built
    } else {
        let mut guard = held::write(db);
        let r = access::within(who, || guard.execute_with(&stmt, &params));
        seq = Some(guard.change_seq());
        let durability = match r {
            Ok(_) => match flush_for(cfg, &mut guard) {
                Ok(d) => d,
                Err(e) => return error_response(&e),
            },
            Err(_) => None,
        };
        drop(guard);
        timing::lap(timing::Phase::Execute);
        if let Err(e) = await_durable(db, durability) {
            return error_response(&e);
        }
        timing::lap(timing::Phase::Durable);
        r
    };
    let resp = match result {
        Ok(resp) => {
            let resp = visible(who, resp);
            statements::rows(counted(&resp));
            with_seq(api::render_any(&resp, fenec_core::VERSION), seq)
        }
        Err(e) => error_response(&e),
    };
    timing::lap(timing::Phase::Render);
    resp
}

/// A keyed statement of `/query`: under the write lock, in a block that
/// keeps its key and answer, beside the database for no index.
fn keyed_query(
    db: &Arc<RwLock<Database>>,
    cfg: &Config,
    who: &Who,
    key: &idempotent::Key,
    stmt: &Statement,
    params: &[fenec_core::value::Value],
) -> Response {
    let mut guard = held::write(db);
    let ttl = cfg.idempotency_ttl.as_millis() as i64;
    if let Some(sent) = idempotent::answered(&guard, key, ttl) {
        return sent;
    }
    if let Err(e) = guard.begin() {
        return error_response(&e);
    }
    let result = access::within(who, || guard.execute_with(stmt, params));
    let resp = match &result {
        Ok(r) => {
            statements::rows(counted(r));
            api::render_any(&visible(who, r.clone()), fenec_core::VERSION)
        }
        Err(e) => error_response(e),
    };
    if let Err(e) = keyed(&mut guard, key, &resp, result.is_ok(), ttl) {
        return error_response(&e);
    }
    let seq = result.is_ok().then(|| guard.change_seq());
    let durability = match result {
        Ok(_) => match flush_for(cfg, &mut guard) {
            Ok(d) => d,
            Err(e) => return error_response(&e),
        },
        Err(_) => None,
    };
    drop(guard);
    if let Err(e) = await_durable(db, durability) {
        return error_response(&e);
    }
    with_seq(resp, seq)
}

/// The rows a response returned or changed, for the statements' counts.
fn counted(resp: &fenec_core::prelude::Response) -> u64 {
    match resp {
        fenec_core::prelude::Response::Rows(rs) => rs.rows.len() as u64,
        fenec_core::prelude::Response::Affected(n) => *n as u64,
        _ => 0,
    }
}

/// Batch: statements in order, under a single write lock, as **one block**:
/// every write in it lands, as one record, or none does
/// ([`Database::execute_block`]), a create, a drop or a `create index`
/// among them put back as they are. The first error puts back what the ones
/// before it did, and says so -- `completed` is 0. A batch holding a
/// `compact` runs each statement on its own instead: it stops at the first
/// error, and `completed` says
/// how many were applied.
fn handle_batch(db: &Arc<RwLock<Database>>, cfg: &Config, req: &Request, who: &Who) -> Response {
    let body = match std::str::from_utf8(&req.body) {
        Ok(b) => b,
        Err(_) => return Response::error(400, "the body is not UTF-8"),
    };
    let stmts = match api::parse_batch(body).and_then(|s| api::exactly_batch(db, body, s)) {
        Ok(v) => v,
        Err(e) => return error_response(&e),
    };
    // Every statement is checked before any runs: a refusal halfway would
    // leave the ones before it applied.
    let stmts = match stmts
        .into_iter()
        .map(|(s, p)| scoped(who, s).map(|s| (s, p)))
        .collect::<fenec_core::error::Result<Vec<_>>>()
    {
        Ok(s) => s,
        Err(e) => return error_response(&e),
    };
    if cfg.read_only && stmts.iter().any(|(s, _)| !s.is_read_only()) {
        return Response::error(403, "the server is in read-only mode");
    }
    if stmts.iter().any(|(s, _)| !s.is_read_only()) {
        metrics::wrote();
    }

    let writes = stmts.iter().any(|(s, _)| !s.is_read_only());
    let key = match writes {
        true => match idempotent::key(req, who) {
            Ok(k) => k,
            Err(refusal) => return refusal,
        },
        false => None,
    };
    let mut guard = held::write(db);
    let block = stmts.iter().all(|(s, _)| s.fits_block());
    if key.is_some() && !block {
        return Response::error(
            400,
            "a batch holding a compact is not one block, and takes no Idempotency-Key",
        );
    }
    let ttl = cfg.idempotency_ttl.as_millis() as i64;
    if let Some(sent) = key
        .as_ref()
        .and_then(|k| idempotent::answered(&guard, k, ttl))
    {
        return sent;
    }
    if block {
        if let Err(e) = guard.begin() {
            return error_response(&e);
        }
    }
    let mut results = Vec::with_capacity(stmts.len());
    for (stmt, params) in &stmts {
        // Measured before each statement, as a lone one is: the batch
        // stops at the first the ceiling refuses, as at an error.
        let r = match over_ceiling(cfg.max_memory, &guard, stmt) {
            Some(why) => Err((507, why)),
            None => access::within(who, || guard.execute_with(stmt, params))
                .map_err(|e| (api::status_of(&e), e.to_string())),
        };
        match r {
            Ok(r) => {
                let r = visible(who, r);
                statements::rows(counted(&r));
                results.push(r)
            }
            Err((status, why)) if block => {
                // Nothing of the block reached the file: none of it is left.
                guard.rollback();
                let at = results.len();
                return api::render_batch_stop(status, &why, 0, at, fenec_core::VERSION);
            }
            Err((status, why)) => {
                // Sync on the error path too: whatever was applied is durable.
                // The statement's error is the one reported.
                if !results.is_empty() {
                    let durability = flush_for(cfg, &mut guard).ok().flatten();
                    drop(guard);
                    let _ = await_durable(db, durability);
                }
                let n = results.len();
                return api::render_batch_stop(status, &why, n, n, fenec_core::VERSION);
            }
        }
    }
    let kept = match &key {
        Some(k) => {
            let resp = api::render_batch(&results, fenec_core::VERSION);
            keyed(&mut guard, k, &resp, true, ttl).map(|_| Some(resp))
        }
        None => guard.commit().map(|_| None),
    };
    let kept = match kept {
        Ok(kept) => kept,
        Err(e) => return error_response(&e),
    };
    let seq = Some(guard.change_seq());
    let durability = match flush_for(cfg, &mut guard) {
        Ok(d) => d,
        Err(e) => return error_response(&e),
    };
    drop(guard);
    if let Err(e) = await_durable(db, durability) {
        return error_response(&e);
    }
    with_seq(
        kept.unwrap_or_else(|| api::render_batch(&results, fenec_core::VERSION)),
        seq,
    )
}

/// Why the data ceiling `max` (bytes, 0 = off) refuses `stmt`, if it does.
///
/// Only a statement that grows the data is stopped: `del` and `compact` are
/// the way out of a database at the ceiling, and reads are unaffected. It is
/// measured before the statement, so the overshoot is at most one. One rule
/// for both listeners: fenec-server held only its own writes to it, and a client
/// writing over HTTP never met it.
pub fn over_ceiling(max: usize, db: &Database, stmt: &Statement) -> Option<String> {
    let grows = matches!(
        stmt,
        Statement::Put { .. } | Statement::Update { .. } | Statement::CreateIndex { .. }
    );
    if max == 0 || !grows {
        return None;
    }
    let used = db.memory_bytes();
    if used < max {
        return None;
    }
    // KiB rather than `0 MiB` for small values.
    let human = |b: usize| match b >= 1 << 20 {
        true => format!("{} MiB", b >> 20),
        false => format!("{} KiB", b >> 10),
    };
    Some(format!(
        "data ceiling exceeded: {} / {}. Writes have stopped; run `del` + \
         `compact` to make room, or raise --max-memory",
        human(used),
        human(max)
    ))
}

/// The answer to a write [`over_ceiling`] refuses: 507, which is what
/// *insufficient storage* is for.
fn refused(why: String) -> Response {
    Response::error(507, &why)
}

/// Under `sync_on_write`, hands the writes over while the write lock is
/// held and returns what the answer has to wait for. The fsync itself runs
/// in [`await_durable`], after the lock is let go: readers do not wait on
/// the disk, and writes arriving together share one fsync.
fn flush_for(cfg: &Config, db: &mut Database) -> fenec_core::error::Result<Option<Durability>> {
    if cfg.sync_on_write {
        db.flush()
    } else {
        Ok(None)
    }
}

/// Waits for the writes `flush_for` handed over, without the lock. A
/// failure is reported to the engine as well, which then refuses every
/// later write as after a failure of its own.
fn await_durable(
    db: &RwLock<Database>,
    durability: Option<Durability>,
) -> fenec_core::error::Result<()> {
    let Some(durable) = durability else {
        return Ok(());
    };
    durable().inspect_err(|e| {
        crate::log!("sync error: {e}");
        db.write().unwrap_or_else(|p| p.into_inner()).fail(e);
    })
}

/// `POST /<name>/near` is a read: taking the write lock would block the other
/// readers for no reason.
fn wants_write(req: &Request) -> bool {
    match req.method {
        Method::Get | Method::Head | Method::Options => false,
        Method::Post => req.segments().last() != Some(&"near"),
        _ => true,
    }
}

/// `Error`'s `Display` also prints the type prefix ("query error: ..."); over
/// HTTP the class is already in the status code, so only the message goes
/// into the body.
fn error_response(e: &Error) -> Response {
    let msg = match e {
        Error::NotFound(m)
        | Error::Exists(m)
        | Error::Duplicate(m)
        | Error::Type(m)
        | Error::Query(m)
        | Error::Corrupt(m)
        | Error::Io(m)
        | Error::Plugin(m)
        | Error::ReadOnly(m)
        | Error::Denied(m)
        | Error::Unmet(m) => m.as_str(),
    };
    Response::error(api::status_of(e), msg)
}

fn cors(resp: Response, cfg: &Config) -> Response {
    match &cfg.cors {
        None => resp,
        Some(origin) => resp
            .header("Access-Control-Allow-Origin", origin)
            .header(
                "Access-Control-Allow-Methods",
                "GET, POST, PATCH, DELETE, OPTIONS",
            )
            // A page's replica sends each write under an `Idempotency-Key`
            // and reads the `Fenec-Seq` of its answer, which tells it when a
            // stream is past the write: refused and unread across origins
            // before, its retries made a row twice and an insert the shape
            // did not hold kept its temporary row until the next seed.
            .header(
                "Access-Control-Allow-Headers",
                "content-type, authorization, idempotency-key, fenec-after, fenec-wait",
            )
            .header(
                "Access-Control-Expose-Headers",
                "fenec-seq, fenec-next, idempotent-replayed",
            )
            .header("Access-Control-Max-Age", "600")
            .header("Vary", "Origin"),
    }
}

/// The token comparison is constant time: an early-exit comparison leaks the
/// length of the correct prefix.
pub(crate) fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_eq_matches_eq() {
        assert!(constant_eq(b"secret", b"secret"));
        assert!(!constant_eq(b"secret", b"secre"));
        assert!(!constant_eq(b"secret", b"secrez"));
        assert!(constant_eq(b"", b""));
    }

    #[test]
    fn loopback_addresses_are_local() {
        assert!(!is_remote("127.0.0.1:8080"));
        assert!(!is_remote("localhost:8080"));
        assert!(is_remote("0.0.0.0:8080"));
    }

    /// The lease runs out between the door's check and the engine's fence:
    /// the write is answered as one the door refused, 503 and come back, not
    /// 403. `lapse()` makes the race certain rather than a matter of timing:
    /// the put passes the door, and its lease lapses as its value is worked
    /// out, before the block lands.
    #[test]
    fn a_write_its_lease_lapses_under_is_answered_503() {
        struct Lapse(Arc<lease::Lease>);
        impl fenec_core::plugin::ScalarFn for Lapse {
            fn call(&self, _: &[Value]) -> fenec_core::error::Result<Value> {
                self.0.grant(0, "e1", None).ok();
                Ok(Value::Text("late".into()))
            }
        }
        let dir = std::env::temp_dir().join(format!(
            "fenec-lease-race-{}-{:?}",
            std::process::id(),
            std::time::Instant::now()
        ));
        let tenants = Tenants::new(&dir).unwrap().with_lease();
        let lease = Arc::clone(tenants.lease().unwrap());
        let held = Arc::clone(&lease);
        let tenants = tenants.with_setup(move |db| {
            db.registry_mut()
                .register_fn("lapse", Arc::new(Lapse(Arc::clone(&held))))
        });
        assert!(lease.grant(60_000, "e1", Some(vec!["acme".into()])).is_ok());
        let t = tenants.create("acme").unwrap();
        let cfg = Config::default();
        let post = |sql: &str| {
            let req = Request {
                method: Method::Post,
                target: "/query".into(),
                path: "/query".into(),
                query: Vec::new(),
                headers: Vec::new(),
                body: format!(r#"{{"query":"{sql}"}}"#).into_bytes(),
                keep_alive: false,
            };
            handle_tenant(&t, &cfg, &req)
        };
        let made = post("create collection notes (title text)");
        assert_eq!(made.status, 200, "{}", String::from_utf8_lossy(&made.body));
        let late = post("put notes {title: lapse()}");
        let body = String::from_utf8_lossy(&late.body);
        assert_eq!(late.status, 503, "{body}");
        assert!(body.contains("lapsed"), "{body}");
        assert!(late.extra.iter().any(|(k, _)| k == "Retry-After"));
        drop(t);
        let _ = std::fs::remove_dir_all(dir);
    }
}
