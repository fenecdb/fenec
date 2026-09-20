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

pub mod api;
pub mod http;
pub mod sse;

use http::{Method, Request, Response};
use sse::Hub;
use std::io::BufReader;
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use fenec_core::prelude::*;

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
        }
    }
}

pub struct Server {
    db: Arc<RwLock<Database>>,
    cfg: Arc<Config>,
    live: Arc<AtomicUsize>,
    hub: Arc<Hub>,
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
            db,
            cfg: Arc::new(cfg),
            live: Arc::new(AtomicUsize::new(0)),
            hub,
        }
    }

    /// Number of live subscriptions (for measurement and tests).
    pub fn live_streams(&self) -> usize {
        self.hub.live()
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

            let db = Arc::clone(&self.db);
            let cfg = Arc::clone(&self.cfg);
            let counter = Arc::clone(&self.live);
            let hub = Arc::clone(&self.hub);
            let spawned = std::thread::Builder::new()
                .name("fenec-http".into())
                .spawn(move || {
                    serve_connection(stream, &db, &cfg, &hub);
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
    Some(
        Response::error(401, "invalid or missing token")
            .header("WWW-Authenticate", "Bearer"),
    )
}

fn is_remote(addr: &str) -> bool {
    match addr.to_socket_addrs() {
        Ok(mut it) => it.any(|a| !a.ip().is_loopback()),
        Err(_) => false,
    }
}

/// A single connection: reads consecutive requests on a keep-alive connection.
fn serve_connection(stream: TcpStream, db: &Arc<RwLock<Database>>, cfg: &Config, hub: &Hub) {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(cfg.idle_timeout);
    let Ok(write_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    let mut out = write_half;

    loop {
        let req = match http::read_request(&mut reader, cfg.max_body) {
            Ok(Some(req)) => req,
            Ok(None) => return,
            Err(http::BadRequest(status, msg)) => {
                let _ = cors(Response::error(status, &msg), cfg).write(&mut out, false, false);
                return;
            }
        };
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
        let keep_alive = req.keep_alive;
        let head_only = req.method == Method::Head;
        let resp = cors(handle(db, cfg, &req), cfg);
        if resp.write(&mut out, keep_alive, head_only).is_err() || !keep_alive {
            return;
        }
    }
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
        if result.is_ok() && cfg.sync_on_write {
            if let Err(e) = guard.sync() {
                return error_response(&e);
            }
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
        if r.is_ok() && cfg.sync_on_write {
            if let Err(e) = guard.sync() {
                return error_response(&e);
            }
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
                if cfg.sync_on_write && !results.is_empty() {
                    let _ = guard.sync();
                }
                return api::render_batch_error(&e, results.len(), fenec_core::VERSION);
            }
        }
    }
    if cfg.sync_on_write {
        if let Err(e) = guard.sync() {
            return error_response(&e);
        }
    }
    api::render_batch(&results, fenec_core::VERSION)
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
        | Error::Plugin(m) => m.as_str(),
    };
    Response::error(api::status_of(e), msg)
}

fn cors(resp: Response, cfg: &Config) -> Response {
    match &cfg.cors {
        None => resp,
        Some(origin) => resp
            .header("Access-Control-Allow-Origin", origin)
            .header("Access-Control-Allow-Methods", "GET, POST, PATCH, DELETE, OPTIONS")
            .header("Access-Control-Allow-Headers", "content-type, authorization")
            .header("Access-Control-Max-Age", "600")
            .header("Vary", "Origin"),
    }
}

/// The token comparison is constant time: an early-exit comparison leaks the
/// length of the correct prefix.
fn constant_eq(a: &[u8], b: &[u8]) -> bool {
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
