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
//! write to one file, so the HTTP endpoint is not a separate binary but a
//! second listener in the same process as `fenec-pg` (`fenec-pg --http`). That
//! keeps the sync policy, the checkpoint and the memory ceiling in one place.
//!
//! There is no TLS: the same rule as `fenec-pg` applies, and a TLS terminator
//! is needed in front of it on an open network.
//!
//! With [`Server::with_tenants`] one listener serves many databases, one
//! file each, under `/t/<tenant>/...` -- the same surface as a single file
//! below the prefix, so a client's base URL is the only thing that changes.
//! See [`tenants`] for why a file per tenant.

pub mod admin;
pub mod api;
pub mod archive;
pub mod http;
pub mod replication;
pub mod sse;
pub mod tenants;

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
    /// `sync` after every write (the equivalent of fenec-pg's `--sync always`).
    pub sync_on_write: bool,
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
            max_streams: 64,
            stream_keepalive: Duration::from_secs(20),
            stream_write_timeout: Duration::from_secs(30),
            change_capacity: fenec_core::changes::DEFAULT_CAPACITY,
            admin_token: None,
            max_import: 1 << 30,
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
}

impl Server {
    /// Builds the server and attaches itself to the database as a **watcher**:
    /// from then on every write -- whether it comes from HTTP or from
    /// `fenec-pg` -- wakes the waiting subscriptions. One process, one writer,
    /// one wake-up point.
    pub fn new(db: Arc<RwLock<Database>>, cfg: Config) -> Server {
        let hub = Hub::new();
        {
            let mut guard = db.write().unwrap_or_else(|e| e.into_inner());
            guard.set_watcher(Arc::clone(&hub) as Arc<dyn Watcher>);
            guard.set_change_capacity(cfg.change_capacity);
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
        Server {
            backend: Backend::Tenants(tenants),
            cfg: Arc::new(cfg),
            live: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Number of live subscriptions (for measurement and tests). Single
    /// database only; a tenant's streams are counted on its own hub.
    pub fn live_streams(&self) -> usize {
        match &self.backend {
            Backend::Single { hub, .. } => hub.live(),
            Backend::Tenants(_) => 0,
        }
    }

    /// Opens the listener. A non-loopback address is not accepted without a
    /// token unless `--insecure` is given.
    pub fn bind(&self) -> std::io::Result<TcpListener> {
        if is_remote(&self.cfg.addr) && self.cfg.token.is_none() && !self.cfg.insecure {
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
        eprintln!(
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

/// Token check. If it returns a response, the request was rejected.
///
/// It stands apart from `handle` because of the subscription path: that path
/// produces no response and takes the connection over -- but it cannot skip
/// authentication.
fn unauthorized(cfg: &Config, req: &Request) -> Option<Response> {
    let token = cfg.token.as_ref()?;
    let given = req
        .header("authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if constant_eq(given.as_bytes(), token.as_bytes()) {
        return None;
    }
    Some(Response::error(401, "invalid or missing token").header("WWW-Authenticate", "Bearer"))
}

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
    let Ok(write_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    let mut out = write_half;

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

        // The database is only ever borrowed from the tenant, never cloned
        // out of it: the tenant's `Arc` count is what says "in use", and a
        // clone of the inner `Arc` would keep the database alive past a
        // close without the registry knowing.
        let tenant = match backend {
            Backend::Single { .. } => None,
            Backend::Tenants(tenants) => match route_tenant(tenants, cfg, &mut req) {
                Ok(t) => Some(t),
                Err(resp) => {
                    let resp = cors(resp, cfg);
                    if resp.write(&mut out, keep_alive, head_only).is_err() || !keep_alive {
                        return;
                    }
                    continue;
                }
            },
        };
        let (db, hub) = match (&tenant, backend) {
            (Some(t), _) => (&t.db, &t.hub),
            (None, Backend::Single { db, hub, .. }) => (db, hub),
            (None, Backend::Tenants(_)) => unreachable!("routed above"),
        };
        if let Backend::Single {
            repl: Some(repl), ..
        } = backend
        {
            if req.segments().first() == Some(&"_replication") {
                // A replica's stream is a body with no end, like a
                // subscription: it takes the connection over.
                let _ = out.set_read_timeout(None);
                match replication::handle(&mut out, db, repl, &req) {
                    None => return,
                    Some(resp) => {
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
        // A subscription cannot go down the ordinary response path: it is a
        // body with unknown `Content-Length` and no end. It takes the
        // connection over and never returns.
        if is_stream(&req) {
            if let Some(deny) = unauthorized(cfg, &req) {
                let _ = cors(deny, cfg).write(&mut out, false, false);
                return;
            }
            // A read timeout is meaningless during the stream: the client
            // sends nothing, and the wait is on a Condvar.
            let _ = out.set_read_timeout(None);
            sse::serve(&mut out, db, cfg, hub, &req);
            return;
        }
        let resp = match &tenant {
            None => handle(db, cfg, &req),
            Some(t) => handle_tenant(t, cfg, &req),
        };
        // Let go of the tenant before writing: a slow client must not keep
        // it from closing.
        drop(tenant);
        let resp = cors(resp, cfg);
        if resp.write(&mut out, keep_alive, head_only).is_err() || !keep_alive {
            return;
        }
    }
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
    if let Some(deny) = unauthorized(cfg, req) {
        return Err(deny);
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
/// tenant answers as a read-only server would, then the refusal is turned
/// into 503 with `Retry-After` -- a freeze is a move in progress, and the
/// right thing for the client is to come back, not to give up.
fn handle_tenant(t: &tenants::Tenant, cfg: &Config, req: &Request) -> Response {
    let _held = t.enter();
    if !t.is_frozen() || cfg.read_only {
        return handle(&t.db, cfg, req);
    }
    let frozen = Config {
        read_only: true,
        ..cfg.clone()
    };
    let resp = handle(&t.db, &frozen, req);
    if resp.status == 403 {
        return Response::error(503, "the tenant is being moved; retry shortly")
            .header("Retry-After", "1");
    }
    resp
}

/// `GET /<name>/changes`
fn is_stream(req: &Request) -> bool {
    req.method == Method::Get && matches!(req.segments().as_slice(), [_, "changes"])
}

/// Turns a request into a response: auth, preflight, routing, execution.
pub fn handle(db: &Arc<RwLock<Database>>, cfg: &Config, req: &Request) -> Response {
    if let Some(deny) = unauthorized(cfg, req) {
        return deny;
    }

    // A preflight request never reaches the database.
    if req.method == Method::Options {
        return Response::empty(204);
    }

    // Raw FenecQL input: its shape is only known after parsing, so it makes
    // its own locking decision.
    if req.method == Method::Post && req.segments() == ["query"] {
        return handle_query(db, cfg, req);
    }

    // Batch: several statements, one lock, one round trip.
    if req.method == Method::Post && req.segments() == ["batch"] {
        return handle_batch(db, cfg, req);
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
        let mut guard = db.write().unwrap_or_else(|e| e.into_inner());
        let routed = match api::route(&guard, req) {
            Ok(r) => r,
            Err(e) => return error_response(&e),
        };
        let result = guard.execute_with(&routed.statement, &[]);
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
        match result {
            Ok(resp) => api::render(&resp, &routed.shape, fenec_core::VERSION),
            Err(e) => error_response(&e),
        }
    } else {
        let guard = db.read().unwrap_or_else(|e| e.into_inner());
        let routed = match api::route(&guard, req) {
            Ok(r) => r,
            Err(e) => return error_response(&e),
        };
        match guard.query(&routed.statement, &[]) {
            Ok(resp) => api::render(&resp, &routed.shape, fenec_core::VERSION),
            Err(e) => error_response(&e),
        }
    }
}

/// Raw FenecQL: parsed first (without a lock), then whether it reads or writes
/// is read off the statement itself.
fn handle_query(db: &Arc<RwLock<Database>>, cfg: &Config, req: &Request) -> Response {
    let body = match std::str::from_utf8(&req.body) {
        Ok(b) => b,
        Err(_) => return Response::error(400, "the body is not UTF-8"),
    };
    let (stmt, params) = match api::parse_query(body) {
        Ok(v) => v,
        Err(e) => return error_response(&e),
    };
    if cfg.read_only && !stmt.is_read_only() {
        return Response::error(403, "the server is in read-only mode");
    }

    let result = if stmt.is_read_only() {
        db.read()
            .unwrap_or_else(|e| e.into_inner())
            .query(&stmt, &params)
    } else {
        let mut guard = db.write().unwrap_or_else(|e| e.into_inner());
        let r = guard.execute_with(&stmt, &params);
        let durability = match r {
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
        r
    };
    match result {
        Ok(resp) => api::render_any(&resp, fenec_core::VERSION),
        Err(e) => error_response(&e),
    }
}

/// Batch: statements in order, **under a single write lock**. It stops at the
/// first error and reports how many were applied -- nothing is rolled back,
/// because fenecdb has no transaction to roll back.
fn handle_batch(db: &Arc<RwLock<Database>>, cfg: &Config, req: &Request) -> Response {
    let body = match std::str::from_utf8(&req.body) {
        Ok(b) => b,
        Err(_) => return Response::error(400, "the body is not UTF-8"),
    };
    let stmts = match api::parse_batch(body) {
        Ok(v) => v,
        Err(e) => return error_response(&e),
    };
    if cfg.read_only && stmts.iter().any(|(s, _)| !s.is_read_only()) {
        return Response::error(403, "the server is in read-only mode");
    }

    let mut guard = db.write().unwrap_or_else(|e| e.into_inner());
    let mut results = Vec::with_capacity(stmts.len());
    for (stmt, params) in &stmts {
        match guard.execute_with(stmt, params) {
            Ok(r) => results.push(r),
            Err(e) => {
                // Sync on the error path too: whatever was applied is durable.
                // The statement's error is the one reported.
                if !results.is_empty() {
                    let durability = flush_for(cfg, &mut guard).ok().flatten();
                    drop(guard);
                    let _ = await_durable(db, durability);
                }
                return api::render_batch_error(&e, results.len(), fenec_core::VERSION);
            }
        }
    }
    let durability = match flush_for(cfg, &mut guard) {
        Ok(d) => d,
        Err(e) => return error_response(&e),
    };
    drop(guard);
    if let Err(e) = await_durable(db, durability) {
        return error_response(&e);
    }
    api::render_batch(&results, fenec_core::VERSION)
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
        eprintln!("sync error: {e}");
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
        | Error::Type(m)
        | Error::Query(m)
        | Error::Corrupt(m)
        | Error::Io(m)
        | Error::Plugin(m)
        | Error::ReadOnly(m) => m.as_str(),
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
            .header(
                "Access-Control-Allow-Headers",
                "content-type, authorization",
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
}
