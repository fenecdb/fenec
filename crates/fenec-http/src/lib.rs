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
/// would have acted on the signal is the one that died. A line written
/// while a request is served ends with its id ([`request_id::log`]).
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {{
        $crate::request_id::log(::std::format_args!($($arg)*));
    }};
}

pub mod accept;
pub mod access;
pub mod admin;
pub mod api;
pub mod archive;
pub mod audit;
pub mod cdc;
pub mod cors;
pub mod crypto;
pub mod dial;
pub mod held;
pub mod http;
pub mod idempotent;
pub mod lease;
pub mod link;
pub mod mcp;
pub mod metrics;
pub mod node_archive;
pub mod node_stream;
pub mod quota;
pub mod replication;
pub mod request_id;
pub mod seal;
pub mod sse;
pub mod statements;
pub mod studio;
pub mod sweep;
pub mod tenants;
pub mod timing;
pub mod trace;
pub mod waits;
pub mod warm;

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
use tenants::Tenants;

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
    /// Set for a request its client marked as a read (`Fenec-Read:
    /// replica`), which runs read-only: its writes are refused in words
    /// that say why ([`read_only_refused`]) rather than as a server taking
    /// none. Never set on a server's own configuration.
    pub marked: bool,
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
    /// What a router marks the requests it forwards with
    /// ([`audit::router_mark`]): made from `admin_token` on a tenant node,
    /// which a router reaches with it. A request bearing it is waited out
    /// at the router after a refusal, keyed by the client it names.
    pub router_mark: Option<String>,
    /// The studio's files, served under `/_studio/` (`--studio`); `None`
    /// answers that path 404.
    pub studio: Option<Arc<studio::Studio>>,
    /// The MCP endpoint, `POST /_mcp`, and what its tools may do (`--mcp`);
    /// `None` answers that path 404.
    pub mcp: Option<Arc<mcp::Options>>,
}

impl Config {
    /// The config with an empty token taken for none. Every check reads a
    /// request carrying no `Authorization` as the empty token, so an empty
    /// one configured -- a variable set and left empty, a flag given "" --
    /// let in whoever sent none (`/_admin/` did, and the router mark made
    /// of it was anyone's to make). Taken for none, a server asks for none
    /// on loopback and refuses an address past it, as with no token at all.
    fn settled(mut self) -> Config {
        self.token = self.token.filter(|t| !t.is_empty());
        self.admin_token = self.admin_token.filter(|t| !t.is_empty());
        self
    }
}

impl Default for Config {
    fn default() -> Config {
        Config {
            addr: "127.0.0.1:8080".into(),
            token: None,
            cors: None,
            read_only: false,
            marked: false,
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
            router_mark: None,
            studio: None,
            mcp: None,
        }
    }
}

pub struct Server {
    backend: Backend,
    cfg: Arc<Config>,
    live: Arc<AtomicUsize>,
    /// The connections' threads a test has fail ([`Server::fail_spawns`]).
    fail_spawns: accept::FailSpawns,
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
        let cfg = cfg.settled();
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
            fail_spawns: Default::default(),
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
        let mut cfg = cfg.settled();
        if cfg.router_mark.is_none() {
            cfg.router_mark = cfg.admin_token.as_deref().map(audit::router_mark);
        }
        if cfg.access.is_some() {
            tenants.check_scoped_writes();
        }
        tenants.shared();
        Server {
            backend: Backend::Tenants(tenants),
            cfg: Arc::new(cfg),
            live: Arc::new(AtomicUsize::new(0)),
            fail_spawns: Default::default(),
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
            cfg: Arc::new(cfg.settled()),
            live: Arc::new(AtomicUsize::new(0)),
            fail_spawns: Default::default(),
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

    /// How many requests wait on a single database for a write they were
    /// sent after (`Fenec-After`, [`sse::Hub::after_waiting`]); a tenant's
    /// are its hub's.
    pub fn after_waiting(&self) -> usize {
        match &self.backend {
            Backend::Single { hub, .. } => hub.after_waiting(),
            Backend::Tenants(_) | Backend::Metrics { .. } => 0,
        }
    }

    /// What the writes held on a single database (`Fenec-Wait`) did.
    pub fn waits(&self) -> waits::Stats {
        match &self.backend {
            Backend::Single { hub, .. } => hub.waits().stats(),
            Backend::Tenants(_) | Backend::Metrics { .. } => waits::Stats {
                held: 0,
                looks: 0,
                runs: 0,
                picks: 0,
            },
        }
    }

    /// Opens the listener. A non-loopback address is not accepted without a
    /// token unless `--insecure` is given ([`exposed`]).
    pub fn bind(&self) -> std::io::Result<TcpListener> {
        let metrics = matches!(self.backend, Backend::Metrics { .. });
        if let Some(refused) = exposed(&self.cfg, metrics) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                refused,
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

        // The thread a refused connection is held on, made now: at the
        // thread limit it could not be made, and every refusal would close
        // its connection as it answered it (`accept::refuse`). And a listen
        // queue past std's 128, which a burst of reconnecting clients
        // overflowed.
        accept::ready();
        accept::widen(&listener);
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
            if self.cfg.max_connections > 0 && live > self.cfg.max_connections {
                self.live.fetch_sub(1, Ordering::SeqCst);
                // Told with the CORS headers as every other answer is: a
                // page on another origin read a refusal without them as
                // the network failing.
                let answer = Response::retry_after(503, "too many connections", 1);
                accept::refuse(stream, cors(answer, &self.cfg), accept::Refusal::Ceiling);
                continue;
            }

            let backend = self.backend.clone();
            let cfg = Arc::clone(&self.cfg);
            let counter = Arc::clone(&self.live);
            let builder = std::thread::Builder::new()
                .name("fenec-http".into())
                .stack_size(CONNECTION_STACK);
            let spawned = accept::spawn(builder, stream, &self.fail_spawns, move |stream| {
                serve_connection(stream, &backend, &cfg);
                counter.fetch_sub(1, Ordering::SeqCst);
            });
            // No thread: answered as the ceiling answers, where it was
            // closed unanswered (`accept`'s comment).
            if let Err(unserved) = spawned {
                self.live.fetch_sub(1, Ordering::SeqCst);
                let answer = cors(Response::retry_after(503, accept::NO_THREAD, 1), &self.cfg);
                accept::no_thread(unserved, answer, "fenec_connections_refused_total");
            }
        }
        Ok(())
    }

    /// Has the next connections' threads fail to be made, as at the
    /// system's thread limit ([`accept::FailSpawns`]): a test's.
    #[doc(hidden)]
    pub fn fail_spawns(&self) -> accept::FailSpawns {
        self.fail_spawns.clone()
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
            audit::verified();
            return Ok(Who::Full);
        }
    }
    if let (Some(access), Some(given)) = (&cfg.access, given) {
        if given.matches('.').count() == 2 {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            return match access.scope(given, now) {
                Ok(scope) => {
                    audit::verified();
                    Ok(Who::Scoped(Arc::new(scope)))
                }
                Err(why) => Err(refuse(why)),
            };
        }
    }
    // Open: everyone is everything, and no token was verified -- a server
    // that asks for none may still hold an admin, a replication or a
    // scrape's token, whose guesses this must not forgive.
    if cfg.token.is_none() && cfg.access.is_none() {
        return Ok(Who::Full);
    }
    Err(refuse("invalid or missing token"))
}

/// `GET /_whoami`: who the server takes the request's token for -- `open`
/// on a server that asks for none, `full` for its own token, `scoped` for a
/// JSON Web Token, with the rules that apply to it (`Scope::summary_into`)
/// -- and where it was asked: a single database, a tenant node's root, or a
/// tenant under it. A client shows a person what their token may do before
/// they find out by a refusal; it grants nothing, and a token the server
/// refuses is answered as everywhere else.
fn whoami(cfg: &Config, req: &Request, node: &str, tenant: Option<&str>) -> Response {
    if !matches!(req.method, Method::Get | Method::Head) {
        return Response::error(405, "/_whoami is read with GET");
    }
    let who = match authenticate(cfg, req) {
        Ok(who) => who,
        Err(deny) => return deny,
    };
    let mut out = String::from("{\"node\":");
    fenec_core::json::escape_into(&mut out, node);
    out.push_str(",\"tenant\":");
    match tenant {
        Some(t) => fenec_core::json::escape_into(&mut out, t),
        None => out.push_str("null"),
    }
    out.push_str(",\"read_only\":");
    out.push_str(if cfg.read_only { "true" } else { "false" });
    out.push_str(",\"kind\":");
    match &who {
        Who::Full if cfg.token.is_none() && cfg.access.is_none() => out.push_str("\"open\""),
        Who::Full => out.push_str("\"full\""),
        Who::Scoped(scope) => {
            out.push_str("\"scoped\",");
            scope.summary_into(&mut out);
        }
    }
    out.push('}');
    Response::json(200, out)
}

/// Whether `req` may read what every statement cost where it is sent: what
/// reads the data there without a scope, or the admin token. A JWT's user
/// is held to its rows, and the statements name every collection.
fn full(cfg: &Config, req: &Request) -> bool {
    matches!(authenticate(cfg, req), Ok(Who::Full)) || metrics::allowed(cfg, req, true)
}

/// The statement as `who` may run it, and in `mask` what its answer must
/// not show `who` (`access::Mask`, [`shown`]): `None` but where a rule
/// names the fields a read shows.
fn scoped(
    who: &Who,
    stmt: Statement,
    params: &[Value],
    mask: &mut Option<access::Mask>,
) -> fenec_core::error::Result<Statement> {
    match who.scope() {
        None => Ok(stmt),
        // A parameter's documents written in, each held to the rules.
        Some(scope) => scope.rewrite_with(stmt.with_documents(params)?, params, mask),
    }
}

/// The statement as `who` may run it over `db`, under the lock the caller
/// holds on it, its mask in `mask` as [`scoped`] gives it, and the token
/// its writes are checked and its rows shown as: where a rule of the token
/// reads another collection, that rule answered there first
/// ([`access::Scope::bound`]), so that the statement holds its list as the
/// engine reads any `in`, [`access::within`] tests a document by it and a
/// row is masked by it -- unanswered, a rule's filter admits no row it is
/// asked of alone. `None` for every other token, whose path is [`scoped`]'s.
fn scoped_under(
    who: &Who,
    db: &Database,
    stmt: Statement,
    params: &[Value],
    mask: &mut Option<access::Mask>,
) -> fenec_core::error::Result<(Statement, Option<Who>)> {
    match who.scope() {
        Some(scope) if who.reaches_out() => {
            let stmt = stmt.with_documents(params)?;
            let bound = Arc::new(scope.bound(db, &[&stmt])?);
            let stmt = bound.rewrite_with(stmt, params, mask)?;
            Ok((stmt, Some(Who::Scoped(bound))))
        }
        _ => Ok((scoped(who, stmt, params, mask)?, None)),
    }
}

/// A write of a token whose rules read other collections, under the write
/// lock on `db`: held to those rules as they answer now
/// ([`scoped_under`]), from `unscoped`, the statement as it was sent --
/// held before the lock to see what it is, its rules' inner `get`s were
/// not answered then -- and its rows masked by them ([`masked`]).
fn run_bound(
    who: &Who,
    db: &mut Database,
    unscoped: &Statement,
    params: &[Value],
) -> fenec_core::error::Result<Response2> {
    let mut mask = None;
    let (stmt, bound) = scoped_under(who, db, unscoped.clone(), params, &mut mask)?;
    let who = bound.as_ref().unwrap_or(who);
    access::within(who, || db.execute_with(&stmt, params))
        .map(|r| masked(who, mask.as_ref(), db, r))
}

/// An answer as `who` is shown it: each row's fields a read rule withholds
/// taken out (`mask`, from [`scoped`]), and [`visible`]. `db` is what
/// answered, for the schemas.
fn shown(who: &Who, mask: Option<&access::Mask>, db: &Database, resp: Response2) -> Response2 {
    visible(who, masked(who, mask, db, resp))
}

/// An answer's rows with the fields `mask` withholds from `who` taken out.
fn masked(who: &Who, mask: Option<&access::Mask>, db: &Database, mut resp: Response2) -> Response2 {
    if let (Some(scope), Some(mask)) = (who.scope(), mask) {
        scope.shown(mask, db, &mut resp);
    }
    resp
}

/// A list of collections, cut to those `who` may read, each without the
/// fields no read rule grants it.
fn visible(who: &Who, resp: Response2) -> Response2 {
    match (who.scope(), resp) {
        (Some(scope), fenec_core::query::Response::Schemas(mut list)) => {
            list.retain(|s| scope.readable(&s.name));
            for s in &mut list {
                if let Some(shown) = scope.shown_schema(s) {
                    *s = shown;
                }
            }
            fenec_core::query::Response::Schemas(list)
        }
        (_, resp) => resp,
    }
}

type Response2 = fenec_core::query::Response;

/// Why an endpoint at `cfg.addr` is refused, or `None`: an address that is
/// not loopback takes a token (`/_metrics` alone, `metrics`, the admin
/// token too) unless `--insecure` is given. A question of the flags alone,
/// which `fenec-server` asks with them, before it opens anything, and
/// [`Server::bind`] again.
pub fn exposed(cfg: &Config, metrics: bool) -> Option<String> {
    // An empty token is no token: a config not yet settled may hold one.
    let set = |t: &Option<String>| t.as_deref().is_some_and(|t| !t.is_empty());
    let guarded = set(&cfg.token) || cfg.access.is_some() || metrics && set(&cfg.admin_token);
    (is_remote(&cfg.addr) && !guarded && !cfg.insecure).then(|| {
        format!(
            "{} is not a loopback address and there is no token.\n\
             The HTTP endpoint does not speak TLS; listening without a\n\
             token on an open network exposes the whole database to\n\
             everyone. Pass `--http-token` (or --insecure if deliberate).",
            cfg.addr
        )
    })
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
    let peer = stream.peer_addr().ok();
    audit::connection("http", peer);
    trace::connection(stream.local_addr().ok(), peer);
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
                // A request that could not be read has an id all the same:
                // its answer is what a client reports.
                request_id::begin(None);
                let _ = cors(Response::error(status, &msg), cfg).write(&mut out, false, false);
                return;
            }
        };
        request_id::begin_request(&req);
        metrics::begin_request();
        if cfg.router_mark.is_some() {
            audit::request(&req, cfg.router_mark.as_deref());
        }
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

        // A browser's preflight carries no `Authorization` -- the Fetch
        // standard sends none -- so it is answered before any token is
        // asked for, a tenant looked up or a lock taken: authenticated, it
        // was 401 on any server with a token, and a page on another origin
        // could send nothing. It reads nothing and says only what the CORS
        // headers already say on every answer.
        if req.method == Method::Options
            && !matches!(backend, Backend::Metrics { .. })
            && cors_allows(cfg, &req)
        {
            let resp = cors(Response::empty(204), cfg);
            if resp.write(&mut out, keep_alive, head_only).is_err() || !keep_alive {
                return;
            }
            continue;
        }

        // The studio's files (`--studio`): static, the same for everyone,
        // and asking no token -- the token is the page's to send with each
        // request it makes, which the rest of this loop answers as any
        // client's. Off, the path is a 404 like an unknown one.
        if studio::is_studio(&req) && !matches!(backend, Backend::Metrics { .. }) {
            let resp = studio::handle(cfg.studio.as_deref(), &req);
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

        // The node's stream of every tenant (`node_stream.rs`), with the
        // node's replication token: a body with no end, as a replica's
        // stream is, which takes the connection over.
        if let Backend::Tenants(tenants) = backend {
            if req.path.starts_with("/_replication") {
                if node_stream(&mut out, tenants, cfg, &req, peer) {
                    continue;
                }
                return;
            }
        }

        // A request from here on is traced: what is above it -- a health
        // check, a scrape, a preflight, the studio's files -- would be most
        // of the spans and none of the time.
        trace::begin(&req);

        // The database is only ever borrowed from the tenant, never cloned
        // out of it: the tenant's `Arc` count is what says "in use", and a
        // clone of the inner `Arc` would keep the database alive past a
        // close without the registry knowing.
        let tenant = match backend {
            Backend::Single { .. } | Backend::Metrics { .. } => None,
            Backend::Tenants(tenants) => match route_tenant(tenants, cfg, &mut req) {
                Ok(t) => {
                    if trace::recording() {
                        trace::attr("fenec.tenant", t.name().to_string());
                    }
                    Some(t)
                }
                Err(resp) => {
                    audit::http(&req, resp.status, peer);
                    trace::end(resp.status);
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
            trace::end(resp.status);
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
        // A read its client marked for a replica runs read-only on every
        // route below ([`marked`]), and a copy of a tenant that has not
        // caught up with its primary yet answers none: a read replica made
        // a moment ago answered from an empty file of a history of its own,
        // a tenant with no collections. Its 503 sends a router home.
        let marked_cfg;
        let cfg = match marked(cfg, &req) {
            Ok(None) => cfg,
            Ok(Some(read_only)) => {
                let behind = tenant.as_ref().filter(|t| !t.ready()).map(|t| {
                    Response::retry_after(
                        503,
                        &format!(
                            "tenant `{}`'s copy here has not caught up with its primary yet: \
                             a read sent without Fenec-Read is answered at its home",
                            t.name()
                        ),
                        1,
                    )
                });
                match behind {
                    None => {
                        marked_cfg = read_only;
                        &marked_cfg
                    }
                    Some(refused) => {
                        audit::http(&req, refused.status, peer);
                        trace::end(refused.status);
                        if cors(refused, cfg)
                            .write(&mut out, keep_alive, head_only)
                            .is_err()
                            || !keep_alive
                        {
                            return;
                        }
                        continue;
                    }
                }
            }
            Err(refused) => {
                audit::http(&req, refused.status, peer);
                trace::end(refused.status);
                if cors(refused, cfg)
                    .write(&mut out, keep_alive, head_only)
                    .is_err()
                    || !keep_alive
                {
                    return;
                }
                continue;
            }
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
                trace::discard();
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
        if req.segments() == ["_whoami"] {
            let node = if tenant.is_some() {
                "tenants"
            } else {
                "single"
            };
            let resp = whoami(cfg, &req, node, tenant.as_ref().map(|t| t.name()));
            audit::http(&req, resp.status, peer);
            trace::end(resp.status);
            if cors(resp, cfg)
                .write(&mut out, keep_alive, head_only)
                .is_err()
                || !keep_alive
            {
                return;
            }
            continue;
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
                // A wait for changes holds a thread as a stream does; one
                // that does not wait is a request in flight. Sent after a
                // write its client made elsewhere (`Fenec-After`), it waits
                // for it as a read does, past its token and before its
                // page: a replica behind answered a page ending before the
                // write with a `Fenec-Seq` behind it too -- read as every
                // write there is -- or a cursor from home 409, past its
                // last write. `since` is no position: a cursor past the
                // database is 409 as before, since a primary that lost
                // writes in a failover never reaches it in this history.
                //
                // The position is looked at first, and a stream of the
                // tenant's taken only while it waits: a page after a write
                // this copy holds is a request in flight.
                (Ok(_), Some(feed)) => {
                    let refused = match req.header("fenec-after").is_some() {
                        true => after(&req, db, hub, || admit(&tenant, true)),
                        false => None,
                    };
                    match refused {
                        Some(refused) => refused,
                        None => match admit(&tenant, cdc_waits(&req)) {
                            Ok(_slot) => cdc::route(db, feed, cfg, &req),
                            Err(refused) => refused,
                        },
                    }
                }
            };
            // A consumer's commit is a tenant's write: asked of the lease
            // again as it is answered, and refused by it 503 to come back.
            let consumer = matches!(req.method, Method::Post | Method::Delete)
                && matches!(req.segments().as_slice(), ["_changes", "consumers", _]);
            let resp = match &tenant {
                Some(t) if consumer => answered_under_lease(t, &req, resp),
                _ => resp,
            };
            // Its `Fenec-Seq` a change of the history the database is on,
            // as every answer carrying one names it.
            let mut resp = resp;
            if resp.seq.is_some() {
                resp.history = http::history_of(&held::read(db));
            }
            if let Some(t) = &tenant {
                quota::noted(&resp, t.name(), &req);
            }
            audit::http(&req, resp.status, peer);
            trace::end(resp.status);
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
            trace::discard();
            let who = match authenticate(cfg, &req) {
                Ok(who) => who,
                Err(deny) => {
                    audit::http(&req, deny.status, peer);
                    let _ = cors(deny, cfg).write(&mut out, false, false);
                    return;
                }
            };
            // Held for as long as the stream lasts: past the tenant's
            // quota of streams, refused before its head.
            let _slot = match admit(&tenant, true) {
                Ok(slot) => slot,
                Err(refused) => {
                    if let Some(t) = &tenant {
                        quota::noted(&refused, t.name(), &req);
                    }
                    audit::http(&req, refused.status, peer);
                    let _ = cors(refused, cfg).write(&mut out, false, false);
                    return;
                }
            };
            // A read timeout is meaningless during the stream: the client
            // sends nothing, and the wait is on a Condvar. A stream sent
            // with `Fenec-After` waits for that write in `serve`, past this
            // slot and the database's own, before its head.
            let _ = out.set_read_timeout(None);
            sse::serve(&mut out, db, cfg, hub, &req, &who);
            return;
        }
        // Reading one's own write on a replica: the request waits until the
        // write its client made on the primary (`Fenec-Seq`) is here -- once
        // `handle` would take it past its token. Waited for first, a guessed
        // token held a connection thread for up to 30 s, enough of them
        // every one `--max-connections` allows, and the 504 after told it
        // where the database stood. A request refused goes on to `handle`
        // at once, which refuses it as any other.
        //
        // The tenant's stream is taken only once it must wait: one taken
        // for every request sent with the header refused, at a tenant
        // whose quota of streams its live queries' subscriptions filled,
        // every read after a write the node held already -- each run of a
        // live query reading near that reached home was 429.
        let waits = req.header("fenec-after").is_some() && admitted(cfg, &req);
        let refused = match waits {
            true => after(&req, db, hub, || admit(&tenant, true)),
            false => None,
        };
        if let Some(refusal) = refused {
            if let Some(t) = &tenant {
                quota::noted(&refusal, t.name(), &req);
            }
            trace::end(refusal.status);
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
        let run = || match &tenant {
            None => handle(db, cfg, &req),
            Some(t) => handle_tenant(t, cfg, &req),
        };
        // A client gone while its request was held: a peek finds the
        // connection closed and nothing more of it read.
        let gone = || {
            let s = reader.get_ref();
            if !reader.buffer().is_empty() || s.set_nonblocking(true).is_err() {
                return false;
            }
            let closed = matches!(s.peek(&mut [0u8; 1]), Ok(0));
            let _ = s.set_nonblocking(false);
            closed
        };
        let hold = tenant.as_ref().map(|t| &t.quota);
        let (resp, waited, held) = respond(run, hub, db, &req, gone, hold);
        if let Some(t) = &tenant {
            quota::noted(&resp, t.name(), &req);
        }
        // A preflight is the browser's, not a statement.
        if req.method != Method::Options {
            // A held request's time is its runs', not its wait: the slow
            // log and the statements' counts would hold every claim that
            // waited for a job.
            let took = started.elapsed().saturating_sub(waited);
            let failed = resp.status >= 400;
            let what = describe(&req);
            let name = tenant.as_ref().map(|t| t.name());
            metrics::record(took, failed, name, || what.clone());
            statements::record(name, &what, took, failed);
            if trace::recording() {
                // An MCP call's statements are noted as `/query`'s are, and
                // its shape is theirs.
                let mcp = cfg.mcp.is_some() && req.segments() == ["_mcp"];
                statements::last_shape(|shape| {
                    let op = if mcp { "mcp" } else { operation(&req, shape) };
                    trace::attr("db.operation.name", op);
                    // FenecQL's shape holds no literal; a REST request's is
                    // its target, whose query string's values are words the
                    // shape keeps, so it has its route alone.
                    if mcp || matches!(req.segments().as_slice(), ["query"] | ["batch"]) {
                        trace::attr("db.query.text", shape.to_string());
                    }
                });
            }
        }
        // Let go of the tenant before writing: a slow client must not keep
        // it from closing.
        drop(tenant);
        audit::http(&req, resp.status, peer);
        let resp = cors(resp, cfg);
        timing::lap(timing::Phase::Books);
        let written = resp.write(&mut out, keep_alive, head_only);
        if held {
            waits::answered();
        }
        trace::end(resp.status);
        if written.is_err() || !keep_alive {
            return;
        }
        http::give_back(resp.body);
        timing::lap(timing::Phase::Write);
        timing::end();
    }
}

/// A request's answer, run again while `Fenec-Wait` holds it ([`waits`]):
/// the answer, how long it was held, and whether it was. Each run is the
/// whole of `run` -- the token asked again, a tenant's gate entered and let
/// go -- so a held request holds no tenant against a move, and a token
/// past its `exp` is refused at the run after.
fn respond(
    run: impl Fn() -> Response,
    hub: &Arc<Hub>,
    db: &RwLock<Database>,
    req: &Request,
    gone: impl Fn() -> bool,
    quota: Option<&Arc<quota::Held>>,
) -> (Response, Duration, bool) {
    // A plan left by a caller of `handle` that holds nothing is not this
    // request's.
    drop(waits::planned());
    let mut resp = run();
    let Some(plan) = waits::planned() else {
        return (resp, Duration::ZERO, false);
    };
    // Asked again: a server going down holds nothing more.
    let Ok(Some(wait)) = waits::asked(req) else {
        return (resp, Duration::ZERO, false);
    };
    // A held write holds a thread as a stream does: past its tenant's
    // quota of held writes it is answered as it ran, and told to come back.
    let _held = match quota.map(|q| q.held()).transpose() {
        Ok(slot) => slot,
        Err(refused) => return (refused, Duration::ZERO, false),
    };
    let held = std::time::Instant::now();
    let until = held + wait;
    let waiting = trace::span("fenec.wait_for_rows");
    let ticket = waits::Ticket::join(hub, plan);
    let mut ran = Duration::ZERO;
    loop {
        match ticket.next(until, db) {
            waits::Next::Run => {
                // Its client went away: the row goes to the next, at once.
                if gone() {
                    ticket.leave(true);
                    break;
                }
                let t = std::time::Instant::now();
                let answer = run();
                ran += t.elapsed();
                // Its tenant's rate of writes let none through: it stays
                // held, and runs once the rate lets one through, rather
                // than be answered the 429 and come back at every group's
                // end. A held claim is charged when a run writes, not as it
                // waits or looks.
                if let Some(wait) = rated_out(&answer, quota) {
                    if std::time::Instant::now() + wait < until {
                        ticket.pause(wait);
                        continue;
                    }
                }
                resp = answer;
                match waits::planned() {
                    Some(plan) => ticket.again(plan),
                    None => {
                        ticket.leave(resp.status < 300);
                        break;
                    }
                }
            }
            waits::Next::End => {
                ticket.leave(false);
                break;
            }
            waits::Next::Closed => {
                ticket.leave(false);
                resp = Response::retry_after(
                    503,
                    "the tenant was closed on this node; retry shortly",
                    1,
                );
                break;
            }
        }
    }
    drop(waiting);
    (resp, held.elapsed().saturating_sub(ran), true)
}

/// How long until `resp`, a tenant's refusal for its rates of writes, lets
/// a write through: `None` for any other answer.
fn rated_out(resp: &Response, quota: Option<&Arc<quota::Held>>) -> Option<Duration> {
    let rated = resp.status == 429
        && resp.extra.iter().any(|(k, v)| {
            k == quota::HEADER
                && matches!(
                    quota::Kind::parse(v),
                    Some(quota::Kind::Writes | quota::Kind::WriteBytes)
                )
        });
    if !rated {
        return None;
    }
    Some(
        quota
            .and_then(|q| q.write_wait())
            .unwrap_or(Duration::from_millis(1)),
    )
}

/// How long a request sent with `Fenec-After` waits for the write it names,
/// unless `Fenec-Wait` says otherwise, and the longest it may.
pub const AFTER_WAIT: Duration = Duration::from_secs(5);
pub const AFTER_LONGEST: Duration = Duration::from_secs(30);

/// A request sent with `Fenec-After: <n>` is served once this database holds
/// change `n` -- a replica, once it has applied the write its client made on
/// the primary and was answered with `Fenec-Seq: <n>` -- or answered 504
/// after the wait, never with what came before it. With `Fenec-History`,
/// as the answer gave it, it is change `n` of that history: a failover
/// forks the history where the copy promoted stood, and a copy left on the
/// old branch holds a change `n` of its own -- on the seq alone it answered
/// a write the client's had replaced. Such a copy waits until it is
/// pointed at the new primary and takes its image. A primary that does not
/// hold it never will -- the write was lost in a failover -- and answers
/// 504 at once, where it stands, for the client to take that as its own.
/// Every route that reads asks it, once its token is taken: a request on
/// `handle`'s way, a subscription before its head ([`sse::serve`]) and
/// `/_changes` before its page -- on a read replica each was answered from
/// before the client's write while the reads beside them waited.
///
/// `slot` is what holds the wait -- a tenant's stream, its quota's -- taken
/// only where the first look finds the write neither held nor out of reach:
/// a read after a write the copy holds, or never will, waits for nothing.
fn after(
    req: &Request,
    db: &RwLock<Database>,
    hub: &sse::Hub,
    slot: impl FnOnce() -> std::result::Result<Option<quota::Slot>, Response>,
) -> Option<Response> {
    let n = req.header("fenec-after")?;
    let Ok(n) = n.trim().parse::<u64>() else {
        return Some(Response::error(400, "Fenec-After expects a change number"));
    };
    let of = match req.header("fenec-history") {
        None => None,
        Some(h) => match http::parse_history(h) {
            Some(f) => Some(f.id.get()),
            None => {
                return Some(Response::error(
                    400,
                    "Fenec-History expects <term>-<id> as an answer's Fenec-History gave it",
                ))
            }
        },
    };
    let wait = req
        .header("fenec-wait")
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(AFTER_WAIT, Duration::from_millis)
        .min(AFTER_LONGEST);
    // Whether this copy holds the write, and whether it ever can: a
    // primary's history grows by its own writes alone, so one that holds
    // the write's history but not as its last -- or not at all -- never
    // will. Of the history it is on, a change past its own is one it may
    // yet reach, as before histories were named.
    let look = || {
        let g = db.read().unwrap_or_else(|e| e.into_inner());
        let (now, h) = (g.change_seq(), g.history());
        let holds = match of {
            None => n <= now,
            Some(id) => h.continues(id, n, now),
        };
        let never = !holds && !h.following && of.is_some_and(|id| id != h.current());
        (holds, never, At::of(&g))
    };
    let first = look();
    let seen = std::cell::Cell::new(Some(first));
    if !(first.0 || first.1) {
        let _slot = match slot() {
            Ok(slot) => slot,
            Err(refused) => return Some(refused),
        };
        let _waiting = trace::span("fenec.wait_for_write");
        hub.reached(
            || {
                let l = look();
                seen.set(Some(l));
                (l.0 || l.1, l.2.seq)
            },
            std::time::Instant::now() + wait,
        );
    }
    let (holds, never, at) = seen.get().expect("looked once at least");
    if holds {
        return None;
    }
    let mut body = format!(
        "{{\"error\":\"change {n} {}\",\"seq\":{}",
        match never {
            true => "is not in this copy's history: a failover replaced it",
            false => "has not reached this server in the wait",
        },
        at.seq
    );
    if let Some(h) = at.history {
        body.push_str(",\"history\":\"");
        http::push_history(&mut body, h);
        body.push('"');
    }
    body.push('}');
    Some(Response::json(504, body))
}

/// A write's answer, with the change it left the database at: a replica
/// sent `Fenec-After` with it serves the read once it holds the write.
fn with_seq(mut resp: Response, at: Option<At>) -> Response {
    if let (true, Some(at)) = (resp.status < 300, at) {
        resp.seq = Some(at.seq);
        resp.history = at.history;
    }
    resp
}

/// Where a database stands, as an answer says it: its change
/// (`Fenec-Seq`) and the history that change is of (`Fenec-History`).
#[derive(Clone, Copy)]
struct At {
    seq: u64,
    history: Option<http::Fork>,
}

impl At {
    fn of(db: &Database) -> At {
        At {
            seq: db.change_seq(),
            history: http::history_of(db),
        }
    }
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

/// What a request does, for a span's `db.operation.name`: a FenecQL
/// statement's first word, or the one a REST route stands for.
fn operation(req: &Request, shape: &str) -> &'static str {
    const WORDS: [&str; 14] = [
        "get",
        "put",
        "insert",
        "set",
        "del",
        "create",
        "drop",
        "alter",
        "compact",
        "explain",
        "collections",
        "begin",
        "commit",
        "rollback",
    ];
    match (req.method, req.segments().as_slice()) {
        (Method::Post, ["batch"]) => "batch",
        (Method::Post, ["query"]) => {
            let first = shape.split_whitespace().next().unwrap_or("");
            WORDS
                .iter()
                .find(|w| first.eq_ignore_ascii_case(w))
                .copied()
                .unwrap_or("query")
        }
        (Method::Post, [_, "near"]) => "near",
        (Method::Get | Method::Head, _) => "get",
        (Method::Post, _) => "insert",
        (Method::Patch | Method::Put, _) => "set",
        (Method::Delete, _) => "del",
        _ => "other",
    }
}

/// `/_replication/...` on a node of tenants: the stream of every tenant,
/// which takes the connection over, or the refusal. Whether the connection
/// goes on to its next request. Out of line and cold: inlined, the stream's
/// whole session landed in the loop every request runs.
#[cold]
#[inline(never)]
fn node_stream(
    out: &mut TcpStream,
    tenants: &Tenants,
    cfg: &Config,
    req: &Request,
    peer: Option<std::net::SocketAddr>,
) -> bool {
    let _ = out.set_read_timeout(None);
    let Some(resp) = node_stream::handle(out, tenants, req) else {
        return false;
    };
    audit::http(req, resp.status, peer);
    if cors(resp, cfg)
        .write(out, req.keep_alive, req.method == Method::Head)
        .is_err()
        || !req.keep_alive
    {
        return false;
    }
    let _ = out.set_read_timeout(cfg.idle_timeout);
    true
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
    // The node's own answer, before a tenant is chosen: which tenants the
    // token's claim names is what a client picks one from.
    if segs.as_slice() == ["_whoami"] {
        return Err(whoami(cfg, req, "tenants", None));
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
        audit::verified();
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
    // A tenant this node has no file of says so in a header besides the
    // words: a router reading a read replica's 404 tells a copy gone from
    // a row or a collection the read did not find, and asks home.
    let t = tenants.get(&name).map_err(|r| match r.0 {
        404 => r.response().header(ABSENT_HEADER, "absent"),
        _ => r.response(),
    })?;
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
    // A request in flight, where the tenant's quota counts them: one held
    // here holds a worker thread, and a tenant past its share is told to
    // come back rather than queued behind the others' work.
    let _flight = match t.quota.request() {
        Ok(slot) => slot,
        Err(refused) => return refused,
    };
    quota::within(&t.quota, || handle_admitted(t, cfg, req))
}

/// [`handle_tenant`], the request let in by its tenant's quota.
fn handle_admitted(t: &tenants::Tenant, cfg: &Config, req: &Request) -> Response {
    let _held = t.enter();
    // Read-only -- the server's mode, or a read its client marked
    // ([`marked`]) -- a request passes a freeze and a lapsed lease, since
    // it can only read: a marked write is refused in words of its own,
    // never turned into the 503 below, or told to come back, the client
    // would send it to the same replica again.
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
    if let Some(why) = refused {
        return match resp.status {
            403 => Response::retry_after(503, &why, 1),
            _ => resp,
        };
    }
    answered_under_lease(t, req, resp)
}

/// A tenant's answer, its lease asked again as the answer is ready: past
/// the write's durability -- its fsync, its wait for the replicas' streams
/// -- and just before it is written out. `handle_admitted`'s, the admin
/// schema's and a `/_changes` consumer's commit.
///
/// A write that landed while the lease let it, and whose lease lapsed
/// before the answer -- a process stopped or starved between the two, a
/// machine suspended, or a durability that waited past the lapse: a stream
/// cut off with the router waits out `SENT_WAIT`, a second, where a lease's
/// tenth is half of one at the default -- may have had its tenant promoted
/// elsewhere meanwhile without it, and answered as a success it was a write
/// acknowledged and lost. Whether it was is not this node's to know: still
/// the primary, it keeps the write. So it says neither ([`outcome_unknown`]).
/// What wrote: a block landed in this request ([`metrics::landed_at`]) --
/// which a `/batch` holding a compact may have done before a later
/// statement of it was refused, told to come back as if nothing had landed
/// -- or a success of a statement that writes ([`metrics::writing`]), which
/// is a keyed write's replay too. Whatever such a batch's later statement
/// was answered -- a 409, a quota's 507, a 503 to come back -- says the
/// statements before it were made, and a 503 invites them again; a 500 is
/// an fsync's that failed, which says the outcome is unknown already. A
/// read answered after the lapse is a read. An MCP write tool's call is
/// answered so as well, its transport's error rather than a tool's result,
/// which would say the write was made.
pub(crate) fn answered_under_lease(t: &tenants::Tenant, req: &Request, resp: Response) -> Response {
    let lapsed = t.writable().err();
    let landed = metrics::landed_at();
    let wrote =
        resp.status != 500 && (landed.is_some() || (resp.status < 300 && metrics::writing()));
    match lapsed {
        Some(_) if wrote => outcome_unknown(t, req, landed, resp),
        lapsed if resp.status == 403 => match refusal_after(t, lapsed) {
            Some(why) => Response::retry_after(503, &why, 1),
            None => resp,
        },
        _ => resp,
    }
}

/// The 500 of a write whose lease lapsed before its answer
/// ([`answered_under_lease`]), as an fsync that failed answers: the outcome
/// unknown, said in the body (`"outcome": "unknown"`) for a client to tell
/// it from an error of the server's, with where the write left the
/// database (`seq`, `history`) and, for a `/batch` that ran a statement at
/// a time, how far it got (`completed`, `at`). A batch's `seq` is its last
/// landed statement's: a read after it answered 200 says every statement
/// that landed survived, and a 504 says nothing of which earlier ones did,
/// which are read back. The position is in the body alone: a client takes
/// an answer's `Fenec-Seq` as its own, and a read near sent after this one
/// waited for a write that may be lost.
fn outcome_unknown(
    t: &tenants::Tenant,
    req: &Request,
    landed: Option<u64>,
    resp: Response,
) -> Response {
    let seq = landed.or(resp.seq);
    let history = resp
        .history
        .or_else(|| http::history_of(&held::read(&t.db)));
    let keyed = req.header("idempotency-key").is_some();
    let mut out = String::from("{\"error\":");
    fenec_core::json::escape_into(&mut out, &lapsed_under(keyed));
    out.push_str(",\"outcome\":\"unknown\"");
    if let Some(n) = seq {
        out.push_str(",\"seq\":");
        http::push_number(&mut out, n);
    }
    if let Some(h) = history {
        out.push_str(",\"history\":\"");
        http::push_history(&mut out, h);
        out.push('"');
    }
    // A batch's progress, as its stop said it.
    if resp.status >= 300 {
        let said = std::str::from_utf8(&resp.body)
            .ok()
            .and_then(|b| fenec_core::json::parse_object(b).ok())
            .unwrap_or_default();
        for (k, v) in said {
            if let ("completed" | "at", Value::Int(n)) = (k.as_str(), v) {
                out.push_str(&format!(",\"{k}\":{n}"));
            }
        }
    }
    out.push('}');
    Response::json(500, out)
}

/// What a write whose lease lapsed before its answer is told to do
/// ([`outcome_unknown`]). Not to read it back at once: until its tenant
/// settles -- the router fails it over a lease and a tenth after the last
/// renewal it counted, or renews the node -- this node answers reads, the
/// write among them, and routers send them here, while the copy promoted
/// may lack it. Sent with an `Idempotency-Key`, the write is sent again
/// with it: a lapsed node refuses it before looking the key up, and once
/// the tenant settles a node still holding the write answers it as made,
/// and a new primary makes it once. Sent with none -- a `compact` or a
/// batch holding one takes none -- it is waited a lease and a tenth for,
/// and only then read back, or read after the answer's position, which a
/// primary without it answers 504: sent at once, a read after that position
/// is answered 200 by this node, whose history holds it, until the router
/// fails the tenant over. The lease's own refusal is not said: it tells a
/// write to go through the router, which is not this one's way back.
fn lapsed_under(keyed: bool) -> String {
    let head = "the write landed on this node, but by the time it was answered this node's \
                lease from its router no longer let it write the tenant: the tenant may have \
                been failed over to another node without the write, or may still be here with it";
    match keyed {
        true => format!(
            "{head}. Send it again with the same Idempotency-Key: it is refused while the \
             tenant settles, then answered as made by a node still holding the write, or made \
             once by a new primary"
        ),
        false => format!(
            "{head}. Until the tenant settles, a lease and a tenth (--auto-failover x 1.1) \
             after this answer, this node answers reads with the write in them whether it \
             survives or not, a read sent with Fenec-After among them. After that, read it back \
             through the router, or send a read with Fenec-After and Fenec-History set to this \
             answer's seq and history, which a primary without the write answers 504; for a \
             batch, seq is its last landed statement's, and a 504 does not say which earlier \
             ones survived"
        ),
    }
}

/// Why this node takes no write for `t` now, if it takes none.
fn refusal(t: &tenants::Tenant) -> Option<String> {
    refusal_after(t, t.writable().err())
}

/// [`refusal`], the lease's answer asked already.
fn refusal_after(t: &tenants::Tenant, lapsed: Option<String>) -> Option<String> {
    match t.is_frozen() {
        true => Some("the tenant is being moved; retry shortly".to_string()),
        false => lapsed,
    }
}

/// What a tenant's quota holds of a request that holds a thread for long:
/// a stream for one that waits -- a subscription, a `/_changes` wait, a
/// `Fenec-After` wait -- a request in flight otherwise. A single database
/// has no tenant, and holds nothing.
fn admit(
    tenant: &Option<Arc<tenants::Tenant>>,
    stream: bool,
) -> std::result::Result<Option<quota::Slot>, Response> {
    let Some(t) = tenant else {
        return Ok(None);
    };
    match stream {
        true => t.quota.stream().map(Some),
        false => t.quota.request().map(Some),
    }
}

/// Whether a `/_changes` read waits for a write (`wait`), holding its
/// thread as a stream does.
fn cdc_waits(req: &Request) -> bool {
    !cdc::page_wait(req).is_zero()
}

/// `GET /<name>/changes`
fn is_stream(req: &Request) -> bool {
    req.method == Method::Get && matches!(req.segments().as_slice(), [_, "changes"])
}

/// Whether `req` is the MCP endpoint's: its own path, a tenant's below its
/// prefix. Its underscore keeps it out of a collection's way, as
/// `/_metrics`'s does. The path is looked at before its segments are made:
/// every other request pays a scan, not a `Vec`.
fn is_mcp(req: &Request) -> bool {
    req.path.contains("_mcp") && req.segments() == ["_mcp"]
}

/// Whether [`handle`] takes `req` past its token -- the MCP endpoint's own
/// checks where it sends it there -- asked before a request waits for
/// anything, so that a refused one waits for nothing. Only a request
/// carrying `Fenec-After` asks it, and `handle` then asks again: a token
/// is 0.27 us a check from the verified cache, a wait up to 30 s.
fn admitted(cfg: &Config, req: &Request) -> bool {
    match is_mcp(req) {
        true => mcp::admit(cfg, req).is_ok(),
        false => authenticate(cfg, req).is_ok(),
    }
}

/// The header a client marks a request it knows only reads with
/// (`Fenec-Read: replica`): a router may answer it from a tenant's read
/// replica near it rather than from its home region (`fenec-shard`'s read
/// replicas, `site/content/docs/sharding-read-replicas.html#read-replicas`), and every
/// server runs it read-only.
pub const READ_HEADER: &str = "fenec-read";

/// Whether a [`READ_HEADER`] value marks a read: the one value a node runs
/// read-only, which a router may so send twice.
pub fn marks_read(value: &str) -> bool {
    value.trim().eq_ignore_ascii_case("replica")
}

/// `Fenec-Tenant: absent` on the 404 of a tenant a node has no file of.
pub const ABSENT_HEADER: &str = "Fenec-Tenant";

/// What [`READ_HEADER`] says, where a request carries it: `Ok(true)` a read
/// marked for a replica, `Err` a value it does not take.
pub fn read_marked(req: &Request) -> std::result::Result<bool, Response> {
    match req.header(READ_HEADER).map(str::trim) {
        None => Ok(false),
        Some(v) if marks_read(v) => Ok(true),
        Some(v) => Err(Response::error(
            400,
            &format!("Fenec-Read takes `replica` (got `{v}`)"),
        )),
    }
}

/// The configuration a request runs under: `cfg` read-only where its
/// client marked it as a read, wherever it lands -- the tenant's home node
/// as much as a replica -- so a write marked by mistake is refused the day
/// it is written, not the day a region gains a replica, and in words that
/// say why rather than as a node taking no write. A replica refuses writes
/// anyway; on the primary the mark would otherwise let a write the client
/// meant to send near land home, and teach it nothing. The one place the
/// mark is read: every route of `serve_connection` past the tenant runs
/// under what it gives -- the MCP endpoint, a consumer's commit, the
/// tenant's freeze and lease, which a read alone may pass
/// ([`handle_admitted`]). [`handle`] called on its own runs `cfg` as it is
/// given. `None` for `cfg` as it is; a value the header does not take is
/// 400.
fn marked(cfg: &Config, req: &Request) -> std::result::Result<Option<Config>, Response> {
    Ok(read_marked(req)?.then(|| Config {
        read_only: true,
        marked: true,
        ..cfg.clone()
    }))
}

/// A write refused by read-only mode, the server's or a marked read's.
pub(crate) fn read_only_refused(cfg: &Config) -> Response {
    match cfg.marked {
        true => Response::error(
            403,
            "a request sent with Fenec-Read: replica only reads, and this one writes: send it \
             without the header, which a router sends to the tenant's home",
        ),
        false => Response::error(403, "the server is in read-only mode"),
    }
}

/// Turns a request into a response: auth, preflight, routing, execution.
pub fn handle(db: &Arc<RwLock<Database>>, cfg: &Config, req: &Request) -> Response {
    // The MCP endpoint asks where a request comes from before its token,
    // and answers in JSON-RPC, 404 on a server without `--mcp`.
    if is_mcp(req) {
        return mcp::http(db, cfg, req);
    }
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
        return read_only_refused(cfg);
    }

    // The write path does its routing under the write lock too: if the lock
    // is released between the schema lookup and execution, a `drop
    // collection` arriving in between leaves the two stages inconsistent.
    // A `POST` of documents is read before it, against the schema it then
    // finds still in place ([`early_put`]).
    if wants_write(req) {
        metrics::wrote();
        // A tenant's write rates (`quota::within`), asked before the lock.
        if let Err(refused) = quota::writing() {
            return refused;
        }
        let key = match idempotent::key(req, &who) {
            Ok(k) => k,
            Err(refusal) => return refusal,
        };
        // Let go of after the write lock, with the documents the put wrote.
        let mut ready = fenec_core::engine::Prepared::default();
        let early = early_put(db, req, &who, &mut ready);
        rest_write(db, cfg, req, &who, key, early.as_ref(), &mut ready)
    } else {
        let guard = held::read(db);
        let routed = match api::route(&guard, req) {
            Ok(r) => r,
            Err(e) => return error_response(&e),
        };
        let mut mask = None;
        let (stmt, bound) = match scoped_under(&who, &guard, routed.statement, &[], &mut mask) {
            Ok(s) => s,
            Err(e) => return error_response(&e),
        };
        let shows = bound.as_ref().unwrap_or(&who);
        let running = trace::span("execute");
        let result = guard.query(&stmt, &[]);
        drop(running);
        match result {
            Ok(resp) => {
                let resp = shown(shows, mask.as_ref(), &guard, resp);
                statements::rows(counted(&resp));
                api::render(&resp, &routed.shape, fenec_core::VERSION)
            }
            Err(e) => error_response(&e),
        }
    }
}

/// A REST write under the write lock, routed there unless [`early_put`]
/// read it before against the schema the collection still holds. `ready`
/// is the caller's, let go of after the lock with the documents the put
/// wrote; the statement is borrowed where it was read, not moved: a
/// `Statement` is a kilobyte, and a copy is a `memcpy` of it.
fn rest_write(
    db: &Arc<RwLock<Database>>,
    cfg: &Config,
    req: &Request,
    who: &Who,
    key: Option<idempotent::Key>,
    early: Option<&Early>,
    ready: &mut fenec_core::engine::Prepared,
) -> Response {
    let guard = held::write(db);
    let ttl = cfg.idempotency_ttl.as_millis() as i64;
    if let Some(sent) = key
        .as_ref()
        .and_then(|k| idempotent::answered(&guard, k, ttl))
    {
        return sent;
    }
    if let Some(e) = early.filter(|e| e.holds(&guard)) {
        let run = Run {
            shape: &e.shape,
            stmt: &e.stmt,
            bound: None,
            mask: e.mask.as_ref(),
        };
        return rest_run(db, cfg, who, key, guard, run, ready);
    }
    let routed = match api::route(&guard, req) {
        Ok(r) => r,
        Err(e) => return error_response(&e),
    };
    let mut mask = None;
    let (stmt, bound) = match scoped_under(who, &guard, routed.statement, &[], &mut mask) {
        Ok(s) => s,
        Err(e) => return error_response(&e),
    };
    let run = Run {
        shape: &routed.shape,
        stmt: &stmt,
        bound,
        mask: mask.as_ref(),
    };
    rest_run(db, cfg, who, key, guard, run, ready)
}

/// A REST write's statement as [`rest_write`] found it: read before the
/// write lock or under it, borrowed where it lies -- a `Statement` is a
/// kilobyte, and gathering the two ways into one value moved it again.
struct Run<'a> {
    shape: &'a api::Shape,
    stmt: &'a Statement,
    /// The token the write is checked and its rows shown as, where its
    /// rules were bound under the lock ([`scoped_under`]).
    bound: Option<Who>,
    mask: Option<&'a access::Mask>,
}

/// [`rest_write`]'s statement run under the write lock `guard`, answered,
/// and made durable once the lock is let go.
fn rest_run(
    db: &Arc<RwLock<Database>>,
    cfg: &Config,
    who: &Who,
    key: Option<idempotent::Key>,
    mut guard: std::sync::RwLockWriteGuard<'_, Database>,
    run: Run<'_>,
    ready: &mut fenec_core::engine::Prepared,
) -> Response {
    let ttl = cfg.idempotency_ttl.as_millis() as i64;
    let Run {
        shape,
        stmt,
        bound,
        mask,
    } = run;
    if let Some(why) = over_ceiling(cfg.max_memory, &guard, stmt) {
        return refused(why);
    }
    if key.is_some() {
        if let Err(e) = guard.begin() {
            return error_response(&e);
        }
    }
    let running = trace::span("execute");
    let shows = bound.as_ref().unwrap_or(who);
    // An insert of a few documents runs as it always did.
    let result = access::within(shows, || match ready.built() {
        0 => guard.execute_with(stmt, &[]),
        _ => guard.execute_ready(stmt, &[], ready, 0),
    })
    .map(|r| shown(shows, mask, &guard, r));
    drop(running);
    let answer = |result: &fenec_core::error::Result<fenec_core::prelude::Response>| match result {
        Ok(resp) => {
            statements::rows(counted(resp));
            api::render(resp, shape, fenec_core::VERSION)
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
    let seq = result.is_ok().then(|| At::of(&guard));
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
}

/// A `POST /<name>` read before the write lock: its body's documents as
/// the statement the write runs, read against the collection's schema as
/// it stood a moment before, and that schema, held. The write lock takes it
/// where the collection still holds that very schema ([`Early::holds`]):
/// [`api::route_put`] reads nothing else, so routed there it would be this
/// statement.
struct Early {
    schema: fenec_core::schema::SchemaCell,
    shape: api::Shape,
    stmt: Statement,
    mask: Option<access::Mask>,
}

impl Early {
    /// Whether `db` holds the schema the documents were read against: a
    /// change to a collection's schema copies it, so the same `Arc` is the
    /// same schema, and while this one is held its allocation is no other's.
    fn holds(&self, db: &Database) -> bool {
        db.collection(&self.schema.name)
            .is_ok_and(|c| Arc::ptr_eq(&c.schema, &self.schema))
    }
}

/// The bytes a `POST /<name>`'s body is read before the write lock from
/// ([`early_put`]) where it holds too few documents to build there: under
/// them its read under the lock is about a microsecond, and reading it
/// before -- a read lock more, the schema's count taken and let go, the
/// collection looked up again under the write lock -- took a row of a text
/// and an int 1.11 -> 1.14-1.16 us in process (the mean of 20 000), and
/// the same with this bound 1.20 -> 1.21. A row with a 128-dim vector,
/// read before, took what it did: 4.17 and 4.12 us at the median.
const EARLY_AT: usize = 1024;

/// A `POST /<name>`'s body read with no lock held, and its documents built
/// and encoded under the read lock where it writes [`PREPARE_AT`] or more
/// ([`Database::prepare`]), for the write lock to take as `/query`'s and
/// `/batch`'s puts are taken: under the write lock, every reader waiting,
/// a body of 1 000 rows was read, checked and built. Nothing for any other
/// write, for a token whose rules read other collections -- its statement
/// is bound under the write lock ([`scoped_under`]), so documents built
/// before it would be another statement's -- and for a body the read
/// refuses, which the write lock routes again for its answer. The
/// documents are built only against the schema the body was read against,
/// asked again under the read lock that builds them: built against one
/// that moved meanwhile, a write lock that found it still in place would
/// take documents read for the one before.
///
/// Nothing either for a body under [`EARLY_AT`] bytes that cannot hold
/// [`PREPARE_AT`] documents -- fewer `{` than that -- which is read under
/// the write lock as before.
///
/// [`PREPARE_AT`]: fenec_core::engine::PREPARE_AT
fn early_put(
    db: &RwLock<Database>,
    req: &Request,
    who: &Who,
    ready: &mut fenec_core::engine::Prepared,
) -> Option<Early> {
    if req.method != Method::Post || who.reaches_out() {
        return None;
    }
    let braces = || req.body.iter().filter(|&&b| b == b'{').count();
    if req.body.len() < EARLY_AT && braces() < fenec_core::engine::PREPARE_AT {
        return None;
    }
    // `/<name>`, as `req.segments()` would find it, with no `Vec` made.
    let name = req.path.trim_matches('/');
    if name.is_empty() || name.contains('/') {
        return None;
    }
    let schema = held::read(db).collection(name).ok()?.schema.clone();
    let routed = api::route_put(&schema, req).ok()?;
    let mut mask = None;
    let stmt = scoped(who, routed.statement, &[], &mut mask).ok()?;
    let stmts = [(&stmt, &[][..])];
    if fenec_core::engine::Prepared::worth(&stmts) {
        let g = held::read(db);
        if g.collection(name)
            .is_ok_and(|c| Arc::ptr_eq(&c.schema, &schema))
        {
            *ready = g.prepare(&stmts);
        }
    }
    Some(Early {
        schema,
        shape: routed.shape,
        stmt,
        mask,
    })
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
    // A scoped token's: the collections it may read, each without the
    // fields no read rule grants it.
    let visible = |db: &Database| -> Vec<fenec_core::schema::Schema> {
        db.collection_names()
            .iter()
            .filter(|n| who.scope().is_none_or(|s| s.readable(n)))
            .filter_map(|n| {
                let c = db.collection(n).ok()?;
                let shown = who.scope().and_then(|s| s.shown_schema(&c.schema));
                Some(shown.unwrap_or_else(|| fenec_core::schema::Schema::clone(&c.schema)))
            })
            .collect()
    };
    let outcome = match (req.method, req.segments().as_slice()) {
        (Method::Get | Method::Head, ["_schema"]) => {
            let schemas = visible(&held::read(db));
            // `?as=fenecql`: the same description with its collections as
            // the statements that make them -- what the studio shows a
            // collection as, written by the engine rather than again in
            // the page, and a body `/_schema/plan` takes back as it is.
            if req.query.iter().any(|(k, v)| k == "as" && v == "fenecql") {
                let mut out = String::from("{\"format\":1,\"fenecql\":");
                fenec_core::json::escape_into(&mut out, &with_id_note(&schemas));
                out.push('}');
                return Response::json(200, out);
            }
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
                return read_only_refused(cfg);
            }
            return apply_schema(db, cfg, body);
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

/// `POST /_admin/tenants/<t>/query`: one read of a tenant, answered as
/// `/query` answers it with the server's own authority, for a router's
/// query across tenants. A statement that writes is refused before it
/// runs, in words of its own, and the read runs in read-only mode as well,
/// which refuses one again: two locks on the one door that lets a token
/// meant for moving tenants near their rows. Counted among the tenant's
/// statements and in the slow log, as a read under its prefix is: it is the
/// tenant's load.
/// The collections as the FenecQL that makes them, after a comment naming
/// the `id` no field declares: shown none, agents reading a schema asked
/// for `rowid` or `_id`, and failed. A comment, so the text is still one
/// `/_schema/plan` takes back as it is; none where there is no collection.
pub(crate) fn with_id_note<S: std::borrow::Borrow<Schema>>(schemas: &[S]) -> String {
    let text = fenec_core::declared::fenecql(schemas);
    match text.is_empty() {
        true => text,
        false => format!(
            "-- every row also has an `id`, an int the database makes: `where id = 1`\n{text}"
        ),
    }
}

pub(crate) fn admin_query(t: &tenants::Tenant, cfg: &Config, req: &Request) -> Response {
    if req.body.len() > cfg.max_body {
        return Response::error(
            413,
            &format!(
                "the body is {} bytes, the ceiling is {}",
                req.body.len(),
                cfg.max_body
            ),
        );
    }
    let started = std::time::Instant::now();
    let resp = {
        // Held against a freeze as any request of the tenant's is: a move
        // waits for the read rather than delete the file under it.
        let _held = t.enter();
        match writes(&req.body) {
            Some(word) => Response::error(
                403,
                &format!("a query across tenants reads, and `{word}` writes"),
            ),
            None => {
                let read_only = Config {
                    read_only: true,
                    ..cfg.clone()
                };
                handle_query(&t.db, &read_only, req, &Who::Full)
            }
        }
    };
    let took = started.elapsed();
    let failed = resp.status >= 400;
    let what = || {
        let mut s = format!("{} {} ", req.method.name(), req.target);
        s.push_str(&String::from_utf8_lossy(&req.body));
        s
    };
    metrics::record(took, failed, Some(t.name()), what);
    statements::record(Some(t.name()), &what(), took, failed);
    resp
}

/// The first word of a `/query` body's statement where it writes; `None`
/// for a read, and for a body that does not parse, which the read's own
/// parse then refuses with its error.
fn writes(body: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?;
    let obj = fenec_core::json::parse_object_listing(text, "params").ok()?;
    let sql = obj.iter().find_map(|(k, v)| match v {
        Value::Text(s) if k == "query" || k == "sql" => Some(s.as_str()),
        _ => None,
    })?;
    let stmt = api::parsed(sql).ok()?;
    match stmt.is_read_only() {
        true => None,
        false => Some(sql.split_whitespace().next().unwrap_or("").to_string()),
    }
}

/// `POST /_schema/apply`, and a tenant's schema as a router creates it
/// (`POST /_admin/tenants/<t>/schema`): the description's migrations, then
/// what only adds, one block under the write lock, synced as a write is.
/// 409 with the plan's refusals, which write nothing.
pub(crate) fn apply_schema(db: &Arc<RwLock<Database>>, cfg: &Config, body: &str) -> Response {
    metrics::wrote();
    if let Err(refused) = quota::writing() {
        return refused;
    }
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
    let seq = At::of(&guard);
    drop(guard);
    if let Err(e) = await_durable(db, durability) {
        return error_response(&e);
    }
    match r {
        Ok(o) if o.plan.refusals.is_empty() => with_seq(Response::json(200, o.json()), Some(seq)),
        Ok(o) => Response::json(409, o.json()),
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
    // A token whose rules read other collections is held to them as they
    // answer under the lock that runs the statement ([`scoped_under`]), from
    // the statement as sent; held to them here, it is checked, planned if
    // held ([`waits`]), and its inner `get`s left for the engine.
    let unscoped = who.reaches_out().then(|| Arc::clone(&stmt));
    // The statement parsed is shared (`api::parse_query`): a scope is ANDed
    // into a copy of it.
    let mut mask = None;
    let stmt = match who.scope() {
        None => stmt,
        Some(_) => match scoped(who, Arc::unwrap_or_clone(stmt), &params, &mut mask) {
            Ok(s) => Arc::new(s),
            Err(e) => return error_response(&e),
        },
    };
    let mask = mask.as_ref();
    if cfg.read_only && !stmt.is_read_only() {
        return read_only_refused(cfg);
    }
    if !stmt.is_read_only() {
        metrics::wrote();
        if let Err(refused) = quota::writing() {
            return refused;
        }
    }
    // `Fenec-Wait`: a `set` or a `del` that writes nothing is held until it
    // can ([`waits`]). Asked of the statement as the token runs it.
    let wait = match waits::asked(req) {
        Err(why) => return Response::error(400, &why),
        Ok(w) => w.is_some() && !stmt.is_read_only(),
    };
    if wait {
        if let Err(e) = waits::holdable(&stmt, &params) {
            return error_response(&e);
        }
    }

    if !stmt.is_read_only() {
        let guard = db.read().unwrap_or_else(|e| e.into_inner());
        if let Some(why) = over_ceiling(cfg.max_memory, &guard, &stmt) {
            return refused(why);
        }
    }
    if let Some(k) = &key {
        let stmt = (&*stmt, mask, unscoped.as_deref());
        return keyed_query(db, cfg, who, k, stmt, &params, wait);
    }
    // The change a write left the database at, for `Fenec-Seq`.
    let mut seq = None;
    // A read's `ETag` ([`etag`]).
    let mut tagged = None;
    let result = if stmt.is_read_only() {
        // A long read is pinned under the read lock and run with none held
        // ([`Database::pin`]), so a writer waits for the pin rather than
        // the read. One that reached for an index the pin left out runs
        // again under the lock, as every read did before.
        let mut pin = true;
        loop {
            // A plain `get` is written out as JSON from the stored
            // documents, under the read lock; anything else is answered as
            // rows and rendered after it.
            let guard = held::read(db);
            // A read its client marked names where this copy stood as it
            // read -- a pinned one, as at its pin -- as a write's answer
            // does: a live query reading near sends its next run after it,
            // which a copy behind this one then waits for, where its rows
            // went back to before a write a run answered from home showed.
            // Every client takes a marked answer's position for nothing of
            // its own: it may be a replica's. An unmarked read names none.
            let at = cfg.marked.then(|| At::of(&guard));
            // A token whose rules read other collections: held to them as
            // they answer under this lock, and its rows shown by them.
            let (rebound, rebound_mask, bound);
            let (stmt, mask, who): (&Statement, _, &Who) = match &unscoped {
                None => (&stmt, mask, who),
                Some(sent) => {
                    let mut m = None;
                    match scoped_under(who, &guard, Statement::clone(sent), &params, &mut m) {
                        Ok((s, b)) => {
                            (rebound, rebound_mask, bound) = (s, m, b);
                            (
                                &rebound,
                                rebound_mask.as_ref(),
                                bound.as_ref().unwrap_or(who),
                            )
                        }
                        Err(e) => return error_response(&e),
                    }
                }
            };
            // `If-None-Match` with the tag of an answer whose collections
            // no write has touched since: 304, the query not run (`poll`).
            // Asked for only: a read that sends none is answered as it
            // was, no tag worked out (the first poll sends `"0"`).
            let asked = req.header("if-none-match");
            // A scoped token's tag is its answer's own ([`content_tag`]):
            // the query runs every time, and 304 says only that its rows
            // are the rows it had. Tagged by the change counter, 304
            // against 200 -- and the 304's speed -- told it when anyone
            // wrote to what it reads, rows it may not see among them, and
            // the tag how many writes the database had taken.
            let by_content = asked.is_some() && who.scope().is_some();
            let tag = match by_content {
                true => None,
                false => asked.and_then(|_| etag(&guard, stmt, body)),
            };
            if let (Some(t), Some(asked)) = (&tag, asked) {
                if unchanged(&guard, stmt, asked) {
                    return Response::json(304, Vec::new()).header("ETag", t);
                }
            }
            let pinned = match pin {
                true => guard.pin(&[(stmt, &params[..])]),
                false => None,
            };
            let mut body = String::from_utf8(http::spare_body()).unwrap_or_default();
            let running = trace::span("execute");
            // Rows with a field masked are rows to mask, not the stored
            // bytes: `Ok(None)` is a `get` the JSON path leaves to `query`.
            let (json, guard) = match &pinned {
                Some(p) => {
                    drop(guard);
                    running.attr("fenec.pinned", true);
                    let json = match mask {
                        Some(_) => Some(Ok(None)),
                        None => p.query_json(stmt, &params, &mut body),
                    };
                    (json, None)
                }
                None => {
                    let json = match mask {
                        Some(_) => Ok(None),
                        None => guard.query_json(stmt, &params, &mut body),
                    };
                    (Some(json), Some(guard))
                }
            };
            match json {
                Some(Ok(Some(n))) => {
                    drop(guard);
                    drop(running);
                    timing::lap(timing::Phase::Execute);
                    statements::rows(n as u64);
                    let mut resp =
                        with_seq(Response::json(200, body).versioned(fenec_core::VERSION), at);
                    if let Some(t) = &tag {
                        resp = resp.header("ETag", t);
                    }
                    if by_content {
                        resp = same_answer(resp, asked);
                    }
                    timing::lap(timing::Phase::Render);
                    return resp;
                }
                Some(Ok(None)) => http::give_back(body.into_bytes()),
                Some(Err(e)) => return error_response(&e),
                None => {
                    http::give_back(body.into_bytes());
                    running.forget();
                    pin = false;
                    continue;
                }
            }
            let r = match (&pinned, &guard) {
                (Some(p), _) => p
                    .query(stmt, &params)
                    .map(|r| r.map(|r| masked(who, mask, p.database(), r))),
                (None, Some(g)) => Some(g.query(stmt, &params).map(|r| masked(who, mask, g, r))),
                (None, None) => unreachable!("a read runs pinned or under the lock"),
            };
            drop(guard);
            let Some(r) = r else {
                running.forget();
                pin = false;
                continue;
            };
            drop(running);
            timing::lap(timing::Phase::Execute);
            tagged = tag;
            seq = at;
            break r;
        }
    } else if let Some(built) = {
        let running = trace::span("execute");
        running.attr("fenec.beside", true);
        let built = Database::maintain(db, &stmt);
        if built.is_none() {
            running.forget();
        }
        built
    } {
        // `create index` and `compact` are built beside the database, with
        // no lock held; the index's record then waits for the disk as any
        // write does.
        let durability = match built {
            Ok(_) => {
                let mut g = db.write().unwrap_or_else(|e| e.into_inner());
                seq = Some(At::of(&g));
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
        // A `put` of many documents builds them beside the readers.
        let mut ready = match &unscoped {
            None => held::prepared(db, &[(&stmt, &params[..])]),
            Some(_) => fenec_core::engine::Prepared::default(),
        };
        let mut guard = held::write(db);
        let before = waits::now_ms();
        let running = trace::span("execute");
        let r = match &unscoped {
            None => access::within(who, || guard.execute_ready(&stmt, &params, &mut ready, 0))
                .map(|r| masked(who, mask, &guard, r)),
            Some(sent) => run_bound(who, &mut guard, sent, &params),
        };
        drop(running);
        if wait && waits::wrote_nothing(&stmt, &r) {
            waits::plan(&guard, &[(&stmt, &params)], before);
        }
        seq = Some(At::of(&guard));
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
            let out = with_seq(api::render_any(&resp, fenec_core::VERSION), seq);
            let out = match &tagged {
                Some(t) => out.header("ETag", t),
                None => out,
            };
            match stmt.is_read_only() && who.scope().is_some() {
                true => same_answer(out, req.header("if-none-match")),
                false => out,
            }
        }
        Err(e) => error_response(&e),
    };
    timing::lap(timing::Phase::Render);
    resp
}

/// A read's tag: the change the database stands at as it answers, `"42"`
/// -- or none for a read whose answer can change with no write: one of a
/// collection whose rows expire, one calling `now()`, one holding an inner
/// `get` (its collections are not counted here). Sent back as
/// `If-None-Match`, it is answered 304 while no write has touched what the
/// read reads ([`unchanged`]): a page polling a query asks the server to
/// run it only once something it reads was written, rather than each
/// viewer holding a stream -- a thread each and a wake-up each at every
/// write (`FenecHttp.live`'s `poll`).
fn etag(db: &Database, stmt: &Statement, text: &str) -> Option<String> {
    let Statement::Select(sel) = stmt else {
        return None;
    };
    if sel.has_subquery() || text.contains("now(") {
        return None;
    }
    let expiring = db.expiring();
    let reads = std::iter::once(sel.collection.as_str()).chain(
        sel.lookup
            .iter()
            .flat_map(|l| l.chain().map(|s| s.collection.as_str())),
    );
    for c in reads {
        if expiring.iter().any(|e| e == c) {
            return None;
        }
    }
    Some(format!("\"{}\"", db.change_seq()))
}

/// A scoped read's tag: a digest of its answer, so that it says nothing a
/// token's rows do not -- not when, nor how often, a collection was
/// written. Strong: the same bytes are the same answer.
fn content_tag(body: &[u8]) -> String {
    let d = crypto::sha256(body);
    let mut t = String::with_capacity(36);
    t.push_str("\"c");
    for b in &d[..16] {
        t.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
        t.push(char::from_digit((b & 15) as u32, 16).unwrap_or('0'));
    }
    t.push('"');
    t
}

/// A scoped read's answer, tagged by its content where a tag was asked
/// for, and 304 with no body where it is the one `asked` names.
fn same_answer(resp: Response, asked: Option<&str>) -> Response {
    let Some(asked) = asked else {
        return resp;
    };
    if resp.status != 200 {
        return resp;
    }
    let tag = content_tag(&resp.body);
    if asked.trim() == tag {
        http::give_back(resp.body);
        return Response::json(304, Vec::new()).header("ETag", &tag);
    }
    resp.header("ETag", &tag)
}

/// Whether no write since the change `asked` names -- an `ETag` [`etag`]
/// gave -- touched a collection `stmt` reads. A tag the change ring no
/// longer reaches, or a collection dropped since, is a change.
fn unchanged(db: &Database, stmt: &Statement, asked: &str) -> bool {
    let Statement::Select(sel) = stmt else {
        return false;
    };
    let Some(since) = asked.trim().trim_matches('"').parse::<u64>().ok() else {
        return false;
    };
    if since > db.change_seq() {
        return false;
    }
    let Some(written) = db.changed_collections_since(since) else {
        return false;
    };
    let reads = std::iter::once(sel.collection.as_str()).chain(
        sel.lookup
            .iter()
            .flat_map(|l| l.chain().map(|s| s.collection.as_str())),
    );
    let hit = reads.into_iter().any(|c| written.iter().any(|w| w == c));
    !hit
}

/// A keyed statement of `/query`: under the write lock, in a block that
/// keeps its key and answer, beside the database for no index.
fn keyed_query(
    db: &Arc<RwLock<Database>>,
    cfg: &Config,
    who: &Who,
    key: &idempotent::Key,
    (stmt, mask, unscoped): (&Statement, Option<&access::Mask>, Option<&Statement>),
    params: &[fenec_core::value::Value],
    wait: bool,
) -> Response {
    let mut ready = match unscoped {
        None => held::prepared(db, &[(stmt, params)]),
        Some(_) => fenec_core::engine::Prepared::default(),
    };
    let mut guard = held::write(db);
    let ttl = cfg.idempotency_ttl.as_millis() as i64;
    if let Some(sent) = idempotent::answered(&guard, key, ttl) {
        return sent;
    }
    if let Err(e) = guard.begin() {
        return error_response(&e);
    }
    let before = waits::now_ms();
    let running = trace::span("execute");
    // A token whose rules read other collections: its rows shown as
    // [`run_bound`] answers them.
    let result = match unscoped {
        None => access::within(who, || guard.execute_ready(stmt, params, &mut ready, 0)),
        Some(sent) => run_bound(who, &mut guard, sent, params),
    };
    drop(running);
    // The answer kept with the key is the one the token is shown.
    let resp = match &result {
        Ok(r) => {
            statements::rows(counted(r));
            let r = match unscoped {
                None => shown(who, mask, &guard, r.clone()),
                Some(_) => visible(who, r.clone()),
            };
            api::render_any(&r, fenec_core::VERSION)
        }
        Err(e) => error_response(e),
    };
    // Held, a run that wrote nothing keeps no key: the run that writes
    // keeps it with its rows, and one sent again with the key meanwhile
    // waits as this one does.
    if wait && waits::wrote_nothing(stmt, &result) {
        guard.rollback();
        waits::plan(&guard, &[(stmt, params)], before);
        return with_seq(resp, Some(At::of(&guard)));
    }
    if let Err(e) = keyed(&mut guard, key, &resp, result.is_ok(), ttl) {
        return error_response(&e);
    }
    let seq = result.is_ok().then(|| At::of(&guard));
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
    // A token whose rules read other collections is held to them as each
    // statement runs, as they answer then ([`run_bound`]): the line before
    // may have changed what they read -- a claim, then the courier
    // followed.
    let unscoped = who.reaches_out().then(|| stmts.clone());
    // Every statement is checked before any runs: a refusal halfway would
    // leave the ones before it applied. What each answer must not show
    // the token, beside it.
    let mut masks = Vec::new();
    let stmts = match stmts
        .into_iter()
        .map(|(s, p)| {
            let mut mask = None;
            let s = scoped(who, s, &p, &mut mask);
            masks.push(mask);
            s.map(|s| (s, p))
        })
        .collect::<fenec_core::error::Result<Vec<_>>>()
    {
        Ok(s) => s,
        Err(e) => return error_response(&e),
    };
    if cfg.read_only && stmts.iter().any(|(s, _)| !s.is_read_only()) {
        return read_only_refused(cfg);
    }
    if stmts.iter().any(|(s, _)| !s.is_read_only()) {
        metrics::wrote();
        if let Err(refused) = quota::writing() {
            return refused;
        }
    }

    let writes = stmts.iter().any(|(s, _)| !s.is_read_only());
    // A batch of reads makes nothing to make once: its key is not asked.
    if !writes {
        return read_batch(db, (&stmts, &masks), unscoped.as_deref(), who, true);
    }
    // `Fenec-Wait`: a batch whose writes are `set`s and `del`s, held while
    // it writes nothing ([`waits`]).
    let wait = match waits::asked(req) {
        Err(why) => return Response::error(400, &why),
        Ok(w) => w.is_some(),
    };
    if wait {
        for (s, p) in &stmts {
            if let Err(e) = waits::holdable(s, p) {
                return error_response(&e);
            }
        }
    }
    let held_plan = |guard: &Database, before: i64| {
        let pairs: Vec<(&Statement, &[Value])> = stmts.iter().map(|(s, p)| (s, &p[..])).collect();
        waits::plan(guard, &pairs, before);
    };
    let key = match idempotent::key(req, who) {
        Ok(k) => k,
        Err(refusal) => return refusal,
    };
    // Let go of after the write lock, with the documents the block wrote.
    let mut ready = match &unscoped {
        None => {
            let pairs: Vec<(&Statement, &[Value])> =
                stmts.iter().map(|(s, p)| (s, &p[..])).collect();
            held::prepared(db, &pairs)
        }
        // Each line bound again under the lock as it runs ([`run_bound`]).
        Some(_) => fenec_core::engine::Prepared::default(),
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
    let before = waits::now_ms();
    let mut results = Vec::with_capacity(stmts.len());
    let running = trace::span("execute");
    running.attr("fenec.statements", stmts.len() as i64);
    for (at, ((stmt, params), mask)) in stmts.iter().zip(&masks).enumerate() {
        // Measured before each statement, as a lone one is: the batch
        // stops at the first the ceiling refuses, as at an error.
        let r = match over_ceiling(cfg.max_memory, &guard, stmt) {
            Some(why) => Err((507, why)),
            None => match &unscoped {
                None => access::within(who, || guard.execute_ready(stmt, params, &mut ready, at))
                    .map(|r| masked(who, mask.as_ref(), &guard, r)),
                Some(sent) => run_bound(who, &mut guard, &sent[at].0, params),
            }
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
                // A `require` unmet wrote nothing: held, it waits.
                if wait && status == 412 {
                    held_plan(&guard, before);
                }
                let at = results.len();
                return batch_stop(status, &why, 0, at);
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
                return batch_stop(status, &why, n, n);
            }
        }
    }
    drop(running);
    // Held, a batch whose writes all wrote nothing is put back, its key
    // kept by none, and waits.
    if wait
        && stmts
            .iter()
            .zip(&results)
            .all(|((s, _), r)| waits::nothing_written(s, r))
    {
        guard.rollback();
        held_plan(&guard, before);
        let seq = Some(At::of(&guard));
        return with_seq(api::render_batch(&results, fenec_core::VERSION), seq);
    }
    let kept = match &key {
        Some(k) => {
            let resp = api::render_batch(&results, fenec_core::VERSION);
            keyed(&mut guard, k, &resp, true, ttl).map(|_| Some(resp))
        }
        None => guard.commit().map(|_| None),
    };
    // Refused as it lands -- a quota past which the block would take the
    // tenant (`quota::limit`) -- the batch stops at its last statement,
    // nothing of it applied, as at an error there.
    let kept = match kept {
        Ok(kept) => kept,
        Err(e @ Error::Quota(_)) => {
            let at = stmts.len().saturating_sub(1);
            return batch_stop(api::status_of(&e), &e.to_string(), 0, at);
        }
        Err(e) => return error_response(&e),
    };
    let seq = Some(At::of(&guard));
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

/// A stopped `/batch`'s answer: where it stopped and how much of it stayed
/// applied, and the quota where one stopped it -- whose refusal reads
/// `quota: <kind>: <why>` as the engine's error says it.
fn batch_stop(status: u16, why: &str, completed: usize, at: usize) -> Response {
    let Some((kind, text)) = why.strip_prefix("quota: ").and_then(quota::split) else {
        return api::render_batch_stop(status, why, completed, at, fenec_core::VERSION);
    };
    let mut out = String::from("{\"error\":");
    fenec_core::json::escape_into(&mut out, text);
    out.push_str(&format!(
        ",\"quota\":\"{}\",\"completed\":{completed},\"at\":{at}}}",
        kind.name()
    ));
    Response::json(status, out)
        .versioned(fenec_core::VERSION)
        .header(quota::HEADER, kind.name())
}

/// A `/batch` whose every statement only reads, as one snapshot: what a
/// reconciliation takes -- every read at the one change `Fenec-Seq` names,
/// no write landing between the first and the last. Under the write lock,
/// as every batch was, a ledger's four reads over a million entries held
/// each transfer up to 350 ms, as long as the snapshot took; under one read
/// lock, readers went on beside it but transfers still waited as long, up
/// to 252 ms. Now the batch is pinned ([`Database::pin`]) and read with no
/// lock held, and a transfer waits for the pin alone; a batch the pin
/// cannot answer -- short, or answered by an index -- runs under one read
/// lock as before. A `require` on a read stops it with 412 and `at`, as in
/// a block: nothing to put back.
/// A `/batch`'s statements and the parameters of each.
type Lines = [(Statement, Vec<Value>)];

fn read_batch(
    db: &Arc<RwLock<Database>>,
    (stmts, masks): (&Lines, &[Option<access::Mask>]),
    unscoped: Option<&Lines>,
    sent_as: &Who,
    pin: bool,
) -> Response {
    let guard = held::read(db);
    // A token whose rules read other collections: every statement held to
    // them as they answer under this lock, the batch's one snapshot, and
    // its rows shown by them.
    let rebound;
    let (stmts, masks, who) = match unscoped {
        None => (stmts, masks, sent_as),
        Some(sent) => match rebind(sent_as, &guard, sent) {
            Ok(r) => {
                rebound = r;
                (&rebound.0[..], &rebound.1[..], &rebound.2)
            }
            Err((at, e)) => {
                let (status, why) = (api::status_of(&e), e.to_string());
                return api::render_batch_stop(status, &why, 0, at, fenec_core::VERSION);
            }
        },
    };
    let pairs: Vec<(&Statement, &[Value])> = stmts.iter().map(|(s, p)| (s, &p[..])).collect();
    if let Some(p) = pin.then(|| guard.pin(&pairs)).flatten() {
        // The pin's own change, of the history the database is on: a pin
        // holds the documents alone.
        let at = At {
            seq: p.change_seq(),
            history: http::history_of(&guard),
        };
        drop(guard);
        let running = trace::span("execute");
        running.attr("fenec.statements", stmts.len() as i64);
        running.attr("fenec.pinned", true);
        let shown = (who, masks, p.database());
        if let Some(resp) = answer_reads(&pairs, shown, at, |s, ps| p.query(s, ps)) {
            return resp;
        }
        // A statement reached for an index the pin left out: the whole
        // batch again under the lock, at the one change that holds then --
        // a token's rules answered again there too.
        running.forget();
        if unscoped.is_some() {
            drop(pairs);
            return read_batch(db, (&[], &[]), unscoped, sent_as, false);
        }
        let guard = held::read(db);
        return locked_reads(&guard, &pairs, masks, who);
    }
    locked_reads(&guard, &pairs, masks, who)
}

/// A read batch's statements bound, the mask of each, and the token as
/// bound ([`rebind`]).
type Rebound = (Vec<(Statement, Vec<Value>)>, Vec<Option<access::Mask>>, Who);

/// A read batch's statements as `who`, whose rules read other collections,
/// may run them over `db`, under its read lock: bound once for all of them
/// ([`access::Scope::bound`]), each with its mask, and the token as bound,
/// which shows their rows. Refused, the statement it stopped at: the
/// first that refuses bound alone.
fn rebind(
    who: &Who,
    db: &Database,
    sent: &Lines,
) -> std::result::Result<Rebound, (usize, fenec_core::error::Error)> {
    let Some(scope) = who.scope() else {
        unreachable!("a token's rules read other collections")
    };
    let all: Vec<&Statement> = sent.iter().map(|(s, _)| s).collect();
    let bound = match scope.bound(db, &all) {
        Ok(b) => Arc::new(b),
        Err(e) => {
            let at = all.iter().position(|s| scope.bound(db, &[s]).is_err());
            return Err((at.unwrap_or(0), e));
        }
    };
    let (mut stmts, mut masks) = (Vec::with_capacity(sent.len()), Vec::new());
    for (at, (s, p)) in sent.iter().enumerate() {
        let mut mask = None;
        let s = s
            .clone()
            .with_documents(p)
            .and_then(|s| bound.rewrite_with(s, p, &mut mask))
            .map_err(|e| (at, e))?;
        stmts.push((s, p.clone()));
        masks.push(mask);
    }
    Ok((stmts, masks, Who::Scoped(bound)))
}

/// A read-only batch under the read lock `guard` is.
fn locked_reads(
    guard: &Database,
    pairs: &[(&Statement, &[Value])],
    masks: &[Option<access::Mask>],
    who: &Who,
) -> Response {
    let running = trace::span("execute");
    running.attr("fenec.statements", pairs.len() as i64);
    let seq = At::of(guard);
    answer_reads(pairs, (who, masks, guard), seq, |s, ps| {
        Some(guard.query(s, ps))
    })
    .expect("a read under the lock answers")
}

/// A read-only batch's answer, each statement run by `run` -- `None` where
/// one of them answered `None`, a pin's refusal to reach past what it
/// holds, before any was counted. Stopped at the first error, as a block.
/// Each answer is shown `who` as its mask says, out of `db`, what ran it.
fn answer_reads(
    pairs: &[(&Statement, &[Value])],
    (who, masks, db): (&Who, &[Option<access::Mask>], &Database),
    seq: At,
    run: impl Fn(
        &Statement,
        &[Value],
    ) -> Option<fenec_core::error::Result<fenec_core::prelude::Response>>,
) -> Option<Response> {
    let mut results = Vec::with_capacity(pairs.len());
    let count = |results: &[fenec_core::prelude::Response]| {
        for r in results {
            statements::rows(counted(r));
        }
    };
    for (at, (stmt, params)) in pairs.iter().enumerate() {
        match run(stmt, params)? {
            Ok(r) => results.push(shown(who, masks[at].as_ref(), db, r)),
            Err(e) => {
                count(&results);
                let why = e.to_string();
                return Some(api::render_batch_stop(
                    api::status_of(&e),
                    &why,
                    0,
                    at,
                    fenec_core::VERSION,
                ));
            }
        }
    }
    count(&results);
    Some(with_seq(
        api::render_batch(&results, fenec_core::VERSION),
        Some(seq),
    ))
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
/// the disk, and writes arriving together share one fsync. Otherwise what
/// writes the buffered writes into the file once there are enough of them
/// ([`Database::write_behind`]), after the lock as well: written as they
/// outgrew the buffer, under it, every reader waited for the disk -- on
/// macOS up to 11 ms each time the file's dirty pages passed 8 MB.
fn flush_for(cfg: &Config, db: &mut Database) -> fenec_core::error::Result<Option<Durability>> {
    if cfg.sync_on_write {
        db.flush()
    } else {
        Ok(db.write_behind())
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
    // The fsync (`--sync always`), and on a primary the wait for its
    // replicas' streams (`replication::Tee`), which open spans of their own.
    let waiting = trace::span("durability");
    durable().inspect_err(|e| {
        waiting.error(e.to_string());
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
        // A quota's refusal names the quota (`quota::limit`).
        Error::Quota(m) => return quota::engine_refusal(m),
    };
    Response::error(api::status_of(e), msg)
}

/// Whether `--http-cors` lets `req`'s origin in ([`cors::allows`]).
fn cors_allows(cfg: &Config, req: &Request) -> bool {
    cors::allows(cfg.cors.as_deref(), req)
}

/// `resp` with the CORS headers `--http-cors` asks for ([`cors::apply`]).
fn cors(resp: Response, cfg: &Config) -> Response {
    cors::apply(resp, cfg.cors.as_deref())
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
            // The thread's flags, as the connection resets them for each.
            metrics::begin_request();
            handle_tenant(&t, &cfg, &req)
        };
        let made = post("create collection notes (title text)");
        assert_eq!(made.status, 200, "{}", String::from_utf8_lossy(&made.body));
        let late = post("put notes {title: lapse()}");
        let body = String::from_utf8_lossy(&late.body);
        assert_eq!(late.status, 503, "{body}");
        assert!(body.contains("lapsed"), "{body}");
        assert!(body.ends_with(r#""retry_after":1}"#), "{body}");
        assert!(late.extra.iter().any(|(k, _)| k == "Retry-After"));
        drop(t);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The lease runs out after a write landed and before it is answered:
    /// the write is not answered as a success, which its tenant failed over
    /// meanwhile would have lost, nor as a refusal, which it was not -- it
    /// is in the file. A `/batch` holding a compact runs each statement on
    /// its own, so the put lands before the read after it lapses the lease;
    /// that read alone, after the lapse, is answered as the read it is.
    #[test]
    fn a_write_answered_after_its_lease_lapsed_is_not_a_success() {
        struct Lapse(Arc<lease::Lease>);
        impl fenec_core::plugin::ScalarFn for Lapse {
            fn call(&self, _: &[Value]) -> fenec_core::error::Result<Value> {
                self.0.grant(0, "e1", None).ok();
                Ok(Value::Int(1))
            }
        }
        let dir = std::env::temp_dir().join(format!(
            "fenec-lease-answer-{}-{:?}",
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
        let grant = || assert!(lease.grant(60_000, "e1", Some(vec!["acme".into()])).is_ok());
        grant();
        let t = tenants.create("acme").unwrap();
        let cfg = Config::default();
        let send = |path: &str, body: String| {
            let req = Request {
                method: Method::Post,
                target: path.into(),
                path: path.into(),
                query: Vec::new(),
                headers: Vec::new(),
                body: body.into_bytes(),
                keep_alive: false,
            };
            // The thread's flags, as the connection resets them for each.
            metrics::begin_request();
            handle_tenant(&t, &cfg, &req)
        };
        let batch = |lines: &[&str]| {
            let lines: Vec<String> = lines
                .iter()
                .map(|q| {
                    let mut line = String::from("{\"query\":");
                    fenec_core::json::escape_into(&mut line, q);
                    line.push('}');
                    line
                })
                .collect();
            send("/batch", lines.join("\n"))
        };
        let ok = batch(&[
            "create collection notes (title text)",
            "put notes {title: \"a\"}",
            "get notes select title limit 1",
        ]);
        assert_eq!(ok.status, 200, "{}", String::from_utf8_lossy(&ok.body));
        let late = batch(&[
            "compact notes",
            "put notes {title: \"b\"}",
            "get notes select lapse() as l limit 1",
        ]);
        let body = String::from_utf8_lossy(&late.body);
        assert_eq!(late.status, 500, "{body}");
        assert!(body.contains("landed"), "{body}");
        assert!(body.contains(r#""outcome":"unknown","seq":"#), "{body}");
        // Its position in the body alone, which no client takes as its own.
        assert!(late.seq.is_none() && late.history.is_none());
        // It is in the file: the lease back, it reads there.
        grant();
        let read = send(
            "/query",
            r#"{"query":"get notes where title = \"b\" count"}"#.into(),
        );
        let read = String::from_utf8_lossy(&read.body);
        assert!(read.contains(r#""count":1"#), "{read}");
        // A read answered after the lapse is a read.
        let after = send(
            "/query",
            r#"{"query":"get notes select lapse() as l limit 1"}"#.into(),
        );
        assert_eq!(
            after.status,
            200,
            "{}",
            String::from_utf8_lossy(&after.body)
        );
        drop(t);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// What a write whose lease lapsed before its answer is told to do: one
    /// sent with an `Idempotency-Key` to send it again with the key -- a
    /// node still holding the write replays it, a new primary makes it
    /// once -- and one sent with none to wait a lease and a tenth before
    /// reading it back, this node answering reads with the write in them
    /// until then. Neither is told to read it back at once.
    #[test]
    fn a_lapsed_write_is_told_to_resend_with_its_key_or_to_wait_to_read() {
        let dir = std::env::temp_dir().join(format!(
            "fenec-lease-told-{}-{:?}",
            std::process::id(),
            std::time::Instant::now()
        ));
        let tenants = Tenants::new(&dir).unwrap().with_lease();
        let lease = Arc::clone(tenants.lease().unwrap());
        assert!(lease.grant(0, "e1", Some(vec!["acme".into()])).is_ok());
        let t = tenants.create("acme").unwrap();
        let told = |key: Option<&str>| {
            let req = Request {
                method: Method::Post,
                target: "/query".into(),
                path: "/query".into(),
                query: Vec::new(),
                headers: key
                    .map(|k| ("idempotency-key".to_string(), k.to_string()))
                    .into_iter()
                    .collect(),
                body: Vec::new(),
                keep_alive: false,
            };
            metrics::begin_request();
            metrics::landed(7);
            let resp = answered_under_lease(&t, &req, Response::json(200, "{}"));
            assert_eq!(resp.status, 500);
            String::from_utf8(resp.body).unwrap()
        };
        let keyed = told(Some("k1"));
        assert!(keyed.contains("same Idempotency-Key"), "{keyed}");
        let bare = told(None);
        assert!(
            bare.contains("a lease and a tenth") && !bare.contains("Idempotency-Key"),
            "{bare}"
        );
        for body in [&keyed, &bare] {
            assert!(body.contains(r#""outcome":"unknown","seq":7"#), "{body}");
            assert!(
                !body.contains("send the write through the router"),
                "{body}"
            );
        }
        drop(t);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn rest_post(target: &str, body: String) -> Request {
        Request {
            method: Method::Post,
            target: target.into(),
            path: target.into(),
            query: Vec::new(),
            headers: Vec::new(),
            body: body.into_bytes(),
            keep_alive: false,
        }
    }

    fn rest_docs(n: usize) -> String {
        let docs: Vec<String> = (0..n)
            .map(|i| format!(r#"{{"k":"k{}","n":{i},"v":[0.1,{i}.3]}}"#, i % 3))
            .collect();
        format!("[{}]", docs.join(","))
    }

    fn rest_early(
        db: &RwLock<Database>,
        req: &Request,
    ) -> (Option<Early>, fenec_core::engine::Prepared) {
        let mut ready = fenec_core::engine::Prepared::default();
        let early = early_put(db, req, &Who::Full, &mut ready);
        (early, ready)
    }

    fn rest_run(db: &mut Database, sql: &str) {
        for s in fenec_ql::parse(sql).unwrap() {
            db.execute(&s).unwrap();
        }
    }

    /// What is read and built before the write lock: a `POST /<name>`'s
    /// body where it may hold [`PREPARE_AT`] documents or is [`EARLY_AT`]
    /// bytes, and its documents where they are [`PREPARE_AT`] or more --
    /// nothing for a small body of a few, another write, a body the read
    /// refuses or a collection not there, which the write lock routes for
    /// their answers.
    ///
    /// [`PREPARE_AT`]: fenec_core::engine::PREPARE_AT
    #[test]
    fn a_rest_insert_is_read_and_built_before_the_write_lock() {
        let mut db = Database::new();
        rest_run(&mut db, "create collection ev (k text, n int, v vector<2>)");
        let db = Arc::new(RwLock::new(db));
        let (early, ready) = rest_early(&db, &rest_post("/ev", rest_docs(16)));
        assert!(early.is_some_and(|e| e.holds(&db.read().unwrap())));
        assert_eq!(ready.built(), 1);
        let small = rest_post("/ev", rest_docs(15));
        assert!(small.body.len() < EARLY_AT);
        let (early, ready) = rest_early(&db, &small);
        assert!(early.is_none());
        assert_eq!(ready.built(), 0);
        let long = format!(r#"[{{"k":"{}"}}, {{"k":"x"}}]"#, "x".repeat(EARLY_AT));
        let (early, ready) = rest_early(&db, &rest_post("/ev", long));
        assert!(early.is_some());
        assert_eq!(ready.built(), 0);
        let mut nope = rest_docs(20);
        nope.insert_str(1, r#"{"k":"x","nope":1},"#);
        let refused = [
            rest_post("/ev", nope),
            rest_post("/ev", format!("[{}", "{".repeat(20))),
            rest_post("/nosuch", rest_docs(20)),
            Request {
                method: Method::Patch,
                ..rest_post("/ev", r#"{"n":1}"#.into())
            },
        ];
        for req in &refused {
            let (early, ready) = rest_early(&db, req);
            assert!(early.is_none(), "{}", req.target);
            assert_eq!(ready.built(), 0);
        }
    }

    /// A body read and its documents built before the write lock against a
    /// schema that moves before the lock is had -- a field added, an index
    /// made, the collection made again with its fields in another order or
    /// its vector a json field -- is read and built again under it: the
    /// file is the one the insert routed and built under the lock writes,
    /// byte for byte. Taken as they were built, the documents would lack the
    /// field added or hold their values in the order the collection had
    /// before; taken as it was read, the statement would hand the json field
    /// the vector's `f32`s, which it refuses for the numbers as written.
    #[test]
    fn a_rest_insert_whose_schema_moved_before_the_lock_is_read_again() {
        let changes = [
            "alter collection ev add field extra text",
            "create index on ev (k) @hash",
            "drop collection ev; create collection ev (n int, k text, v vector<2>)",
            "drop collection ev; create collection ev (k text, n int, v json)",
        ];
        let dir = std::env::temp_dir().join(format!("fenec-rest-moved-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (c, change) in changes.iter().enumerate() {
            let a_path = dir.join(format!("{c}-a.fenec"));
            let b_path = dir.join(format!("{c}-b.fenec"));
            let _ = std::fs::remove_file(&a_path);
            let _ = std::fs::remove_file(&b_path);
            let mut a = fenec_core::fs::open_mapped(&a_path).unwrap();
            let mut b = fenec_core::fs::open_mapped(&b_path).unwrap();
            rest_run(&mut a, "create collection ev (k text, n int, v vector<2>)");
            rest_run(&mut b, "create collection ev (k text, n int, v vector<2>)");
            let b = Arc::new(RwLock::new(b));
            let req = rest_post("/ev", rest_docs(40));
            let (early, mut ready) = rest_early(&b, &req);
            assert_eq!(ready.built(), 1);
            rest_run(&mut a, change);
            rest_run(&mut b.write().unwrap(), change);
            assert!(
                !early.as_ref().unwrap().holds(&b.read().unwrap()),
                "{change}"
            );
            let cfg = Config::default();
            let resp = rest_write(&b, &cfg, &req, &Who::Full, None, early.as_ref(), &mut ready);
            let body = String::from_utf8_lossy(&resp.body);
            assert_eq!(resp.status, 201, "{change}: {body}");
            let routed = api::route(&a, &req).unwrap();
            a.execute_with(&routed.statement, &[]).unwrap();
            let b = Arc::into_inner(b).unwrap().into_inner().unwrap();
            let all = fenec_ql::parse_one("get ev order id").unwrap();
            assert_eq!(a.query(&all, &[]).unwrap(), b.query(&all, &[]).unwrap());
            drop((a, b));
            let (x, y) = (
                std::fs::read(&a_path).unwrap(),
                std::fs::read(&b_path).unwrap(),
            );
            assert!(x == y, "{change}: {} and {} bytes differ", x.len(), y.len());
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
