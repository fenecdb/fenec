//! # fenec-shard
//!
//! A router in front of several `fenec-pg --dir` nodes. Each tenant lives
//! whole on one node, in a file of its own; the router knows which node
//! and forwards `/t/<tenant>/...` there byte for byte.
//!
//! ```text
//! client -- /t/acme/notes --> fenec-shard -- /t/acme/notes --> node n1
//!                                 |                              acme.fenec
//!                                 +--> node n2                   beta.fenec
//! ```
//!
//! **Why tenants and not hash partitioning.** A tenant on one node means
//! every query runs where all of its data is: `near`, `match`, `lookup` and
//! `count` keep their single-database meaning, ids and the change sequence
//! stay the tenant's own, and the router never parses a query. Splitting one
//! collection across nodes would need a merge for every read shape and
//! global BM25 statistics for `match`; that is a different feature, and a
//! tenant that outgrows a node is the case it would be for.
//!
//! **The router does not understand the requests it forwards.** It reads
//! the head and the `Content-Length` body, picks the node and copies the
//! answer back. An SSE subscription is copied until either side closes, so
//! the stream format lives in one place, the node.
//!
//! What it owns is the directory (tenant -> node) and three operations that
//! change it: place a new tenant, move one, delete one. See [`directory`]
//! and the `/_shard/` endpoints in [`Router::admin`]. With `--auto-failover`
//! it also leases each node the tenants it places there, and fails a node
//! over once its lease has certainly lapsed ([`lease`]).

pub mod directory;
pub mod lease;
pub mod metrics;
pub mod upstream;

use directory::{Directory, Node, State};
use fenec_core::prelude::*;
use fenec_http::http::{self, Method, Request, Response};
use fenec_http::replication::{self, Replication};
use metrics::Route;
use std::collections::HashSet;
use std::io::{BufReader, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use upstream::Pool;

pub struct Config {
    pub addr: String,
    /// Token for `/_shard/`. The data requests carry whatever the nodes'
    /// `--http-token` expects; the router passes `Authorization` through.
    pub token: Option<String>,
    pub insecure: bool,
    /// Every connection is a thread, as in the nodes.
    pub max_connections: usize,
    pub max_body: usize,
    pub idle_timeout: Option<Duration>,
    /// Connect and read bound when talking to a node.
    pub upstream_timeout: Duration,
    /// Whether a tenant created on a node in no pair gets a replica of its
    /// own on another node (`--replicas`): spread over the nodes rather
    /// than kept whole on an idle standby.
    pub replicas: bool,
    /// The lease each node holds over its tenants (`--auto-failover`): the
    /// router renews it every third of this, and fails a node over once a
    /// tenth past it has gone by unrenewed. `None`: failover is by hand.
    pub auto_failover: Option<Duration>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            addr: "127.0.0.1:8090".into(),
            token: None,
            insecure: false,
            max_connections: 1000,
            max_body: 64 << 20,
            idle_timeout: Some(Duration::from_secs(60)),
            upstream_timeout: Duration::from_secs(60),
            replicas: false,
            auto_failover: None,
        }
    }
}

pub struct Router {
    dir: RwLock<Directory>,
    /// The directory's own replication: the feed a standby reads, and the
    /// follower a standby runs. `None` without --replication-token.
    repl: Option<Arc<Replication>>,
    pool: Pool,
    cfg: Config,
    /// Tenants with a create, move or delete in progress: a second
    /// operation on the same tenant is refused rather than interleaved.
    busy: Mutex<HashSet<String>>,
    live: AtomicUsize,
    /// Each node's lease, under automatic failover, and the pool its grants
    /// go through: bounded by a third of the lease rather than by
    /// `upstream_timeout`, so a node that does not answer cannot hold up
    /// the others' renewals until theirs lapse too.
    leases: Option<(lease::Leases, Pool)>,
}

/// An operation failure, shaped as an HTTP status and message.
struct Fail(u16, String);

type Outcome<T> = std::result::Result<T, Fail>;

impl Router {
    pub fn new(dir: Directory, cfg: Config) -> Arc<Router> {
        Router::over(dir, cfg, None)
    }

    /// A router whose directory is replicated: a standby follows it and
    /// forwards from the same placements, and takes over when promoted.
    pub fn replicated(dir: Directory, cfg: Config, repl: Arc<Replication>) -> Arc<Router> {
        Router::over(dir, cfg, Some(repl))
    }

    fn over(dir: Directory, cfg: Config, repl: Option<Arc<Replication>>) -> Arc<Router> {
        Arc::new(Router {
            dir: RwLock::new(dir),
            repl,
            pool: Pool::new(cfg.upstream_timeout),
            busy: Mutex::new(HashSet::new()),
            live: AtomicUsize::new(0),
            leases: cfg
                .auto_failover
                .map(|term| (lease::Leases::new(term), Pool::new(term / 3))),
            cfg,
        })
    }

    /// Opens the listener. As with the nodes, a non-loopback address needs
    /// a token: `/_shard/` can move and delete every tenant.
    pub fn bind(&self) -> std::io::Result<TcpListener> {
        let remote = match self.cfg.addr.to_socket_addrs() {
            Ok(mut it) => it.any(|a| !a.ip().is_loopback()),
            Err(_) => false,
        };
        if remote && self.cfg.token.is_none() && !self.cfg.insecure {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "{} is not a loopback address and there is no --token: \
                     /_shard/ would let anyone move and delete tenants",
                    self.cfg.addr
                ),
            ));
        }
        TcpListener::bind(&self.cfg.addr)
    }

    pub fn serve_on(self: &Arc<Router>, listener: TcpListener) -> std::io::Result<()> {
        fenec_http::metrics::started();
        fenec_http::log!(
            "fenec-shard {} listening on: http://{}  [{} node(s), {} tenant(s)]",
            fenec_core::VERSION,
            listener.local_addr()?,
            self.read_dir().nodes().len(),
            self.read_dir().tenants().len(),
        );
        for (t, p) in self.read_dir().tenants() {
            if p.state == State::Moving {
                fenec_http::log!(
                    "tenant `{t}` was being moved when the router stopped; it is served \
                     from `{}`, and moving it again clears the state",
                    p.node
                );
            }
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
            let router = Arc::clone(self);
            let spawned = std::thread::Builder::new()
                .name("fenec-shard".into())
                .spawn(move || {
                    router.connection(stream);
                    router.live.fetch_sub(1, Ordering::SeqCst);
                });
            if spawned.is_err() {
                self.live.fetch_sub(1, Ordering::SeqCst);
            }
        }
        Ok(())
    }

    /// Starts the leasing thread under automatic failover: every third of a
    /// lease it renews every node's, and fails over a node whose lease has
    /// certainly lapsed. Nothing without `--auto-failover`.
    pub fn start_leasing(self: &Arc<Router>) -> std::io::Result<()> {
        let Some((leases, _)) = &self.leases else {
            return Ok(());
        };
        let every = leases.term / 3;
        fenec_http::log!(
            "leasing each node its tenants for {:?}: a node unrenewed for {:?} is failed over",
            leases.term,
            lease::lapse(leases.term)
        );
        let router = Arc::clone(self);
        std::thread::Builder::new()
            .name("fenec-lease".into())
            .spawn(move || loop {
                std::thread::sleep(every);
                router.lease_round();
            })?;
        Ok(())
    }

    /// One round: every node's lease renewed, each on a thread of its own so
    /// that one that does not answer holds up no other, then the nodes whose
    /// lease has certainly lapsed failed over.
    pub fn lease_round(&self) {
        let Some((leases, _)) = &self.leases else {
            return;
        };
        // A standby router's directory follows the primary's, which leases.
        if self.read_dir().following() {
            leases.standby();
            return;
        }
        let lists = self.read_dir().primaries();
        std::thread::scope(|s| {
            for (node, list) in &lists {
                leases.know(node);
                s.spawn(move || self.grant(node, list));
            }
        });
        for node in leases.lapsed(Instant::now()) {
            fenec_http::log!(
                "node `{node}` renewed no lease in {:?}: failing its tenants over",
                lease::lapse(leases.term)
            );
            let (status, why) = match self.failover(&node) {
                Ok(r) => (r.status, String::from_utf8_lossy(&r.body).into_owned()),
                Err(Fail(status, why)) => (status, why),
            };
            metrics::failed_over(status == 200);
            fenec_http::log!("failover of `{node}`: {status} {why}");
            // The nodes its tenants went to take their writes now, not at
            // the next round.
            let lists = self.read_dir().primaries();
            std::thread::scope(|s| {
                for (node, list) in &lists {
                    s.spawn(move || self.grant(node, list));
                }
            });
        }
    }

    /// Grants `node` its lease at once, over what the directory places there
    /// now: after a create or a move, so a tenant takes writes on the node it
    /// was placed on before the next round.
    fn lease_now(&self, node: &str) {
        if self.leases.is_none() {
            return;
        }
        let list = self.read_dir().primaries().remove(node).unwrap_or_default();
        self.grant(node, &list);
    }

    /// Sends `node` its lease over `primaries`: the list itself only when
    /// the node may not hold it, and again with it when the node says it
    /// does not. A node that answers after it was failed over has a repair
    /// run, which has its copies follow its tenants' new primaries.
    fn grant(&self, node: &str, primaries: &[String]) {
        let Some((leases, pool)) = &self.leases else {
            return;
        };
        let Some(n) = self.read_dir().node(node).cloned() else {
            return;
        };
        let epoch = lease::epoch_of(primaries);
        let mut with_list = leases.epoch(node).as_deref() != Some(epoch.as_str());
        loop {
            let mut body = format!(
                "{{\"ms\":{},\"epoch\":{}",
                leases.term.as_millis(),
                quote(&epoch)
            );
            if with_list {
                let names: Vec<String> = primaries.iter().map(|t| quote(t)).collect();
                body.push_str(&format!(",\"primaries\":[{}]", names.join(",")));
            }
            body.push('}');
            let answer =
                match pool.call(&n.addr, "POST", "/_admin/lease", &n.token, body.as_bytes()) {
                    Ok((200, _)) => lease::Answer::Taken,
                    Ok((412, _)) => lease::Answer::NeedsList,
                    Ok((404 | 409, _)) => lease::Answer::Refuses,
                    _ => lease::Answer::Silent,
                };
            if leases.answered(node, &epoch, &answer, Instant::now()) {
                fenec_http::log!(
                    "node `{node}` answers again: its copies are to follow the tenants' new primaries"
                );
                let r = self.repair();
                fenec_http::log!("repair: {} {}", r.status, String::from_utf8_lossy(&r.body));
            }
            match answer {
                lease::Answer::NeedsList if !with_list => with_list = true,
                _ => return,
            }
        }
    }

    fn read_dir(&self) -> std::sync::RwLockReadGuard<'_, Directory> {
        self.dir.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write_dir(&self) -> std::sync::RwLockWriteGuard<'_, Directory> {
        self.dir.write().unwrap_or_else(|e| e.into_inner())
    }

    fn connection(&self, stream: TcpStream) {
        let _ = stream.set_nodelay(true);
        let _ = stream.set_read_timeout(self.cfg.idle_timeout);
        let Ok(mut out) = stream.try_clone() else {
            return;
        };
        let _open = metrics::Connection::open();
        let mut reader = BufReader::new(stream);
        loop {
            let req = match http::read_request(&mut reader, self.cfg.max_body) {
                Ok(Some(r)) => r,
                Ok(None) => return,
                Err(http::BadRequest(status, msg)) => {
                    let _ = Response::error(status, &msg).write(&mut out, false, false);
                    return;
                }
            };
            let arrived = Instant::now();
            // Everything but a forwarded request is counted here, as it is
            // answered; `forward` counts its own, a stream at its head.
            let answer = |out: &mut TcpStream, route: Route, resp: Response| {
                let sent = resp
                    .write(out, req.keep_alive, req.method == Method::Head)
                    .is_ok();
                metrics::request(route, resp.status, arrived.elapsed());
                sent && req.keep_alive
            };
            // A standby's directory moves as the primary's writes arrive:
            // the maps are read again before the request is answered from
            // them, and only then.
            if self.read_dir().stale() {
                if let Err(e) = self.write_dir().refresh() {
                    fenec_http::log!("could not read the directory: {e}");
                }
            }
            let keep = match req.segments().first() {
                Some(&"t") => self.forward(&req, &mut out, arrived),
                Some(&"_replication") => {
                    let Some(repl) = self.repl.clone() else {
                        let _ = Response::error(404, "this router has no --replication-token")
                            .write(&mut out, false, false);
                        metrics::request(Route::Replication, 404, arrived.elapsed());
                        return;
                    };
                    // The directory's database, not the router's lock: a
                    // stream lasts as long as the replica stays connected.
                    let db = self.read_dir().db().clone();
                    match replication::handle(&mut out, &db, &repl, &req) {
                        // The stream took the connection over and has ended:
                        // counted, not timed.
                        None => {
                            metrics::counted(Route::Replication, 200);
                            return;
                        }
                        Some(resp) => answer(&mut out, Route::Replication, resp),
                    }
                }
                Some(&"_shard") => answer(&mut out, Route::Shard, self.admin(&req)),
                Some(&"_metrics") => answer(&mut out, Route::Metrics, self.metrics(&req)),
                _ => answer(
                    &mut out,
                    Route::Other,
                    Response::error(
                        404,
                        "the router serves /t/<tenant>/..., /_shard/ and /_metrics",
                    ),
                ),
            };
            if !keep {
                return;
            }
        }
    }

    // ------------------------------------------------------------ forwarding

    /// Forwards one request and copies the answer back, and counts it.
    /// Returns whether the client connection can carry another request.
    fn forward(&self, req: &Request, out: &mut TcpStream, arrived: Instant) -> bool {
        let head_only = req.method == Method::Head;
        let reply = |out: &mut TcpStream, resp: Response| {
            let sent = resp.write(out, req.keep_alive, head_only).is_ok();
            metrics::request(Route::Tenant, resp.status, arrived.elapsed());
            sent && req.keep_alive
        };
        let segs = req.segments();
        let tenant = segs.get(1).copied().unwrap_or("");
        let addr = {
            let dir = self.read_dir();
            let node = dir
                .placement(tenant)
                .and_then(|p| dir.node(&p.node).map(|n| (p.node.clone(), n.addr.clone())));
            match node {
                Some(n) => n,
                // A standby whose maps have not come from the primary yet
                // cannot say a tenant does not exist: 404 tells a client to
                // stop asking, this to ask again.
                None if !dir.arrived() => {
                    return reply(
                        out,
                        Response::error(503, "the directory has not arrived from the primary yet")
                            .header("Retry-After", "1"),
                    )
                }
                None => {
                    return reply(
                        out,
                        Response::error(404, &format!("no tenant `{tenant}` in the directory")),
                    )
                }
            }
        };
        let (node, addr) = addr;

        let headers: Vec<(String, String)> = req
            .headers
            .iter()
            .filter(|(k, _)| !hop_by_hop(k) && !k.eq_ignore_ascii_case("host"))
            .cloned()
            .collect();
        let sent = Instant::now();
        let answer =
            match self
                .pool
                .send(&addr, req.method.name(), &req.target, &headers, &req.body)
            {
                Ok(a) => a,
                Err(e) => {
                    metrics::unreachable(&node);
                    return reply(
                        out,
                        Response::error(
                            502,
                            &format!("node `{node}` ({addr}) did not answer: {e}"),
                        ),
                    );
                }
            };

        let status = answer.status;
        let mut head = format!("HTTP/1.1 {status} {}\r\n", http::reason(status));
        for (k, v) in &answer.headers {
            if !hop_by_hop(k) && !k.eq_ignore_ascii_case("content-length") {
                head.push_str(&format!("{k}: {v}\r\n"));
            }
        }

        if let Some(length) = answer.content_length() {
            let body = match answer.read_body(&self.pool, head_only) {
                Ok(b) => b,
                Err(e) => {
                    metrics::unreachable(&node);
                    return reply(
                        out,
                        Response::error(502, &format!("node `{node}` broke off: {e}")),
                    );
                }
            };
            metrics::upstream(sent.elapsed());
            // A HEAD answer states the length of the body it leaves out.
            head.push_str(&format!(
                "Content-Length: {}\r\nConnection: {}\r\n\r\n",
                if head_only { length } else { body.len() },
                if req.keep_alive {
                    "keep-alive"
                } else {
                    "close"
                }
            ));
            let written = out
                .write_all(head.as_bytes())
                .and_then(|_| out.write_all(&body))
                .and_then(|_| out.flush());
            metrics::request(Route::Tenant, status, arrived.elapsed());
            return written.is_ok() && req.keep_alive;
        }

        // No length: a stream. It is copied as it arrives until either side
        // closes, and the client connection ends with it; it is counted,
        // and timed to its head.
        metrics::upstream(sent.elapsed());
        head.push_str("Connection: close\r\n\r\n");
        let written = out.write_all(head.as_bytes());
        metrics::request(Route::Tenant, status, arrived.elapsed());
        if written.is_err() {
            return false;
        }
        let _ = out.set_read_timeout(None);
        if let Ok(mut stream) = answer.into_stream() {
            let _ = std::io::copy(&mut stream, out);
        }
        false
    }

    // ------------------------------------------------------------------ admin

    /// ```text
    /// GET    /_shard/nodes                     [{name, addr}]
    /// PUT    /_shard/nodes/<n>   {addr, token} add or change a node
    /// DELETE /_shard/nodes/<n>                 refused while it holds tenants
    /// GET    /_shard/tenants                   [{name, node, state}]
    /// PUT    /_shard/tenants/<t> [{node}]      place and create; least disk wins
    /// DELETE /_shard/tenants/<t>               delete on the node, then forget
    /// POST   /_shard/tenants/<t>/move {to}     move to another node
    /// PUT    /_shard/nodes/<n> {addr, token, standby}  standby: the node
    ///                                       whose --replica-of follows this
    ///                                       one, where its tenants are copied
    /// POST   /_shard/nodes/<n>/failover      promote this node's tenants on
    ///                                       its standby -- or each on its own
    ///                                       replica -- and route them there
    /// POST   /_shard/replicas                  give every tenant in no pair
    ///                                       that has no replica one
    /// ```
    pub fn admin(&self, req: &Request) -> Response {
        if let Some(refused) = self.refused(req) {
            return refused;
        }
        let body = match body_fields(req) {
            Ok(b) => b,
            Err(e) => return Response::error(400, &e),
        };
        let segs = req.segments();
        // A standby's directory is a replica's file: it takes no write of
        // its own, so a change belongs on the primary. The reads below are
        // answered from the maps the primary's writes filled.
        let writes = !matches!(
            (req.method, &segs[1..]),
            (Method::Get, ["nodes"]) | (Method::Get, ["tenants"])
        );
        if writes && self.read_dir().following() {
            return Response::error(
                409,
                "this router follows another: send /_shard/ changes to the primary, \
                 or promote this one (POST /_replication/promote)",
            );
        }
        let result = match (req.method, &segs[1..]) {
            (Method::Get, ["nodes"]) => Ok(self.list_nodes()),
            (Method::Put, ["nodes", n]) => self.set_node(n, &body),
            (Method::Delete, ["nodes", n]) => self.remove_node(n),
            (Method::Post, ["nodes", n, "failover"]) => self.failover(n),
            (Method::Post, ["replicas"]) => Ok(self.repair()),
            (Method::Get, ["tenants"]) => Ok(self.list_tenants()),
            (Method::Put, ["tenants", t]) => self.create(t, field(&body, "node")),
            (Method::Delete, ["tenants", t]) => self.delete(t),
            (Method::Post, ["tenants", t, "move"]) => match field(&body, "to") {
                Some(to) => self.relocate(t, to),
                None => Err(Fail(400, "the body needs {\"to\": \"<node>\"}".into())),
            },
            _ => Err(Fail(404, "no such endpoint under /_shard/".into())),
        };
        result.unwrap_or_else(|Fail(status, msg)| Response::error(status, &msg))
    }

    /// The 401 a request without the router's `--token` gets, where it has
    /// one: `/_shard/`, and `/_metrics`, which names the nodes.
    fn refused(&self, req: &Request) -> Option<Response> {
        let token = self.cfg.token.as_ref()?;
        let given = req
            .header("authorization")
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");
        (!constant_eq(given.as_bytes(), token.as_bytes())).then(|| {
            Response::error(401, "invalid or missing token").header("WWW-Authenticate", "Bearer")
        })
    }

    /// `GET /_metrics`: the counters ([`metrics`]), what the directory
    /// holds, and on a router replicated or following, its replication.
    fn metrics(&self, req: &Request) -> Response {
        if let Some(refused) = self.refused(req) {
            return refused;
        }
        if req.method != Method::Get && req.method != Method::Head {
            return Response::error(405, "/_metrics is read with GET");
        }
        let mut out = fenec_http::metrics::Text::new();
        fenec_http::metrics::head(&mut out);
        metrics::counters(&mut out);
        let seq = {
            let dir = self.read_dir();
            let (by_node, moving) = dir.load_by_node();
            out.family("fenec_router_nodes", "gauge", "Nodes in the directory.");
            out.sample("fenec_router_nodes", &[], dir.nodes().len());
            out.family(
                "fenec_router_tenants",
                "gauge",
                "Tenants placed on each node.",
            );
            for (node, n) in &by_node {
                out.sample("fenec_router_tenants", &[("node", node)], n);
            }
            out.family(
                "fenec_router_replicas",
                "gauge",
                "Tenants' replicas of their own each node holds.",
            );
            for (node, n) in dir.replicas_by_node() {
                out.sample("fenec_router_replicas", &[("node", node)], n);
            }
            out.family(
                "fenec_router_tenants_moving",
                "gauge",
                "Tenants a move began on and has not recorded done: served from where they were.",
            );
            out.sample("fenec_router_tenants_moving", &[], moving);
            if let Some((leases, _)) = &self.leases {
                out.family(
                    "fenec_router_lease_age_seconds",
                    "gauge",
                    "Seconds since each node last took its lease: past the lease and a tenth, it is failed over.",
                );
                for (node, age, _) in leases.ages() {
                    out.sample("fenec_router_lease_age_seconds", &[("node", &node)], age);
                }
                out.family(
                    "fenec_router_node_failed_over",
                    "gauge",
                    "1 for a node failed over on its own and not answering again since.",
                );
                for (node, _, lost) in leases.ages() {
                    out.sample(
                        "fenec_router_node_failed_over",
                        &[("node", &node)],
                        lost as u8,
                    );
                }
            }
            out.family(
                "fenec_router_following",
                "gauge",
                "1 on a standby router, whose directory follows the primary's.",
            );
            out.sample("fenec_router_following", &[], dir.following() as u8);
            let db = dir.db().read().unwrap_or_else(|e| e.into_inner());
            db.change_seq()
        };
        if let Some(repl) = &self.repl {
            repl.metrics(&mut out, seq);
        }
        Response {
            status: 200,
            body: out.finish().into_bytes(),
            content_type: "text/plain; version=0.0.4; charset=utf-8",
            extra: Vec::new(),
        }
    }

    fn list_nodes(&self) -> Response {
        let dir = self.read_dir();
        let items: Vec<String> = dir
            .nodes()
            .iter()
            .map(|(name, n)| match dir.standby(name) {
                None => format!("{{\"name\":{},\"addr\":{}}}", quote(name), quote(&n.addr)),
                Some(s) => format!(
                    "{{\"name\":{},\"addr\":{},\"standby\":{}}}",
                    quote(name),
                    quote(&n.addr),
                    quote(s)
                ),
            })
            .collect();
        Response::json(200, format!("[{}]", items.join(",")))
    }

    fn list_tenants(&self) -> Response {
        let dir = self.read_dir();
        let items: Vec<String> = dir
            .tenants()
            .iter()
            .map(|(name, p)| {
                let replica = match dir.replica(name) {
                    Some(r) => format!(",\"replica\":{}", quote(r)),
                    None => String::new(),
                };
                format!(
                    "{{\"name\":{},\"node\":{},\"state\":\"{}\"{replica}}}",
                    quote(name),
                    quote(&p.node),
                    if p.state == State::Moving {
                        "moving"
                    } else {
                        "active"
                    }
                )
            })
            .collect();
        Response::json(200, format!("[{}]", items.join(",")))
    }

    fn set_node(&self, name: &str, body: &[(String, Value)]) -> Outcome<Response> {
        fenec_http::tenants::check_name(name).map_err(|r| Fail(r.0, r.1))?;
        let (Some(addr), Some(token)) = (field(body, "addr"), field(body, "token")) else {
            return Err(Fail(
                400,
                "the body needs {\"addr\": .., \"token\": ..}".into(),
            ));
        };
        let node = Node {
            addr: addr.into(),
            token: token.into(),
        };
        let standby = field(body, "standby").map(str::to_string);
        if let Some(s) = &standby {
            if s == name {
                return Err(Fail(400, "a node cannot be its own standby".into()));
            }
            if self.read_dir().node(s).is_none() {
                return Err(Fail(404, format!("no node `{s}` to be the standby")));
            }
        }
        // A node is checked before it is recorded: an address or token that
        // does not work would otherwise surface on the first placement.
        let (status, _) = self
            .pool
            .call(&node.addr, "GET", "/_admin/stats", &node.token, b"")
            .map_err(|e| Fail(502, format!("{addr} did not answer: {e}")))?;
        if status != 200 {
            return Err(Fail(
                502,
                format!("{addr} refused its admin token ({status})"),
            ));
        }
        let mut dir = self.write_dir();
        dir.set_node(name, node).map_err(internal)?;
        if let Some(s) = &standby {
            dir.set_standby(name, Some(s)).map_err(internal)?;
        }
        let placed: Vec<String> = dir
            .tenants()
            .into_iter()
            .filter(|(_, p)| p.node == name)
            .map(|(t, _)| t)
            .collect();
        drop(dir);
        // The standby gets a following copy of every tenant already here: a
        // pair recorded after the tenants were placed -- a node rejoining as
        // the standby of the one its tenants failed over to -- copied
        // nothing over, and a rejoined node's own files, primaries' files,
        // took no write from anyone.
        let mut unfollowed = Vec::new();
        if standby.is_some() {
            for t in &placed {
                let base = format!("/_admin/tenants/{t}");
                let why = self
                    .on_standby(name, "PUT", &base)
                    .or_else(|| self.on_standby(name, "POST", &format!("{base}/follow")));
                if let Some(w) = why {
                    fenec_http::log!("tenant `{t}` has no replica yet: {w}");
                    unfollowed.push(quote(t));
                }
            }
        }
        Ok(Response::json(
            201,
            format!(
                "{{\"node\":{},\"unreplicated\":[{}]}}",
                quote(name),
                unfollowed.join(",")
            ),
        ))
    }

    /// The call that keeps a node's standby in step: a tenant created here
    /// is created there so the replica has a file to follow into, and one
    /// deleted here is deleted there. Returns what went wrong, if anything;
    /// a standby that is down does not stop the operation, it only leaves
    /// the tenant without a replica until someone repairs it.
    fn on_standby(&self, node: &str, method: &str, target: &str) -> Option<String> {
        let (name, n) = {
            let dir = self.read_dir();
            let s = dir.standby(node)?.to_string();
            let n = dir.node(&s).cloned()?;
            (s, n)
        };
        // "Already as asked": a create finding the tenant there (409), a
        // delete finding it gone (404). A delete answered 409 is a tenant
        // still in use, which stays -- not one deleted.
        let done = |status: u16| {
            status < 300
                || (method == "PUT" && status == 409)
                || (method == "DELETE" && status == 404)
        };
        match self.pool.call(&n.addr, method, target, &n.token, b"") {
            Ok((status, _)) if done(status) => None,
            Ok((status, body)) => Some(format!(
                "standby `{name}` answered {status}: {}",
                String::from_utf8_lossy(&body).trim()
            )),
            Err(e) => Some(format!("standby `{name}` did not answer: {e}")),
        }
    }

    /// Where a tenant on `primary` would keep its replica: the node holding
    /// the fewest of `primary`'s replicas, then the fewest tenants and
    /// replicas, besides `primary` and those in a pair -- a standby's files
    /// follow its node's, and a paired node's tenants are replicated by its
    /// standby. Fewest overall alone had every tenant of one node follow on
    /// the same other: a tie went by name each time, and a failover of the
    /// node put all its tenants there. Among `holding`, the nodes with a
    /// copy of the tenant already, first: its old primary after a failover
    /// follows from where it is, and nothing is left behind there.
    fn replica_node(&self, primary: &str, holding: &[String]) -> Option<String> {
        let dir = self.read_dir();
        let (tenants, _) = dir.load_by_node();
        let replicas = dir.replicas_by_node();
        let from = dir.replicas_from(primary);
        let held = |n: &str| tenants.get(n).unwrap_or(&0) + replicas.get(n).unwrap_or(&0);
        let free =
            |n: &&String| n.as_str() != primary && dir.standby(n).is_none() && !dir.is_standby(n);
        let least = |it: &mut dyn Iterator<Item = &String>| {
            it.min_by_key(|n| (*from.get(n.as_str()).unwrap_or(&0), held(n), n.to_string()))
                .cloned()
        };
        least(
            &mut dir
                .nodes()
                .keys()
                .filter(free)
                .filter(|n| holding.contains(n)),
        )
        .or_else(|| least(&mut dir.nodes().keys().filter(free)))
    }

    /// Gives `tenant`, on `primary`, a replica on `on`: the tenant created
    /// there -- or found there, a copy a failover left -- and made to follow
    /// `primary`. Recorded once it follows, not before.
    fn attach(&self, tenant: &str, primary: &str, on: &str) -> std::result::Result<(), String> {
        let (p, r) = {
            let dir = self.read_dir();
            let node = |n: &str| dir.node(n).cloned().ok_or(format!("no node `{n}`"));
            (node(primary)?, node(on)?)
        };
        let base = format!("/_admin/tenants/{tenant}");
        let from = format!("{{\"from\":{}}}", quote(&format!("http://{}", p.addr)));
        for (method, target, body, ok) in [
            ("PUT", base.clone(), &b""[..], &[201u16, 409][..]),
            (
                "POST",
                format!("{base}/follow"),
                from.as_bytes(),
                &[200][..],
            ),
        ] {
            match self.pool.call(&r.addr, method, &target, &r.token, body) {
                Ok((status, _)) if ok.contains(&status) => {}
                Ok((status, body)) => {
                    return Err(format!(
                        "`{on}` answered {status}: {}",
                        String::from_utf8_lossy(&body).trim()
                    ))
                }
                Err(e) => return Err(format!("`{on}` did not answer: {e}")),
            }
        }
        self.write_dir()
            .set_replica(tenant, Some(on))
            .map_err(|e| message(&e))
    }

    /// Takes a tenant's replica off its node and out of the directory. A
    /// node that does not answer keeps its copy, which follows nothing the
    /// router routes to; the record goes either way.
    fn detach(&self, tenant: &str) -> Option<String> {
        let (on, node) = {
            let dir = self.read_dir();
            let on = dir.replica(tenant)?.to_string();
            let node = dir.node(&on).cloned();
            (on, node)
        };
        let why = node.and_then(|n| {
            let target = format!("/_admin/tenants/{tenant}");
            match self.pool.call(&n.addr, "DELETE", &target, &n.token, b"") {
                Ok((204 | 404, _)) => None,
                Ok((status, body)) => Some(format!(
                    "`{on}` answered {status}: {}",
                    String::from_utf8_lossy(&body).trim()
                )),
                Err(e) => Some(format!("`{on}` did not answer: {e}")),
            }
        });
        match self.write_dir().set_replica(tenant, None) {
            Err(e) => Some(message(&e)),
            Ok(()) => why,
        }
    }

    /// A replica for a tenant created or moved onto `primary`, where the
    /// router gives them (`--replicas`) and `primary` is in no pair: the
    /// node it went on, or why none did -- the tenant goes on without one
    /// until a repair.
    fn replicate(
        &self,
        tenant: &str,
        primary: &str,
    ) -> Option<std::result::Result<String, String>> {
        if !self.cfg.replicas || self.read_dir().standby(primary).is_some() {
            return None;
        }
        Some(match self.replica_node(primary, &[]) {
            None => Err("there is no other node to keep it".into()),
            Some(on) => self.attach(tenant, primary, &on).map(|_| on),
        })
    }

    /// `POST /_shard/replicas`: every tenant in no pair, not moving, without
    /// a replica, is given one -- on a node that holds a copy already where
    /// there is one, so a node back from a failover has its old primaries
    /// follow the new ones rather than lie there.
    fn repair(&self) -> Response {
        let holding: Vec<(String, Vec<String>)> = {
            let nodes: Vec<(String, Node)> = self
                .read_dir()
                .nodes()
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            nodes
                .into_iter()
                .filter_map(|(name, n)| {
                    let (status, body) = self
                        .pool
                        .call(&n.addr, "GET", "/_admin/tenants", &n.token, b"")
                        .ok()?;
                    (status == 200).then(|| (name, names_in(&body)))
                })
                .collect()
        };
        let wanting: Vec<(String, String)> = {
            let dir = self.read_dir();
            dir.tenants()
                .into_iter()
                .filter(|(t, p)| {
                    p.state == State::Active
                        && dir.replica(t).is_none()
                        && dir.standby(&p.node).is_none()
                })
                .map(|(t, p)| (t, p.node))
                .collect()
        };
        let (mut done, mut failed) = (Vec::new(), Vec::new());
        for (tenant, primary) in wanting {
            let Ok(_claim) = self.claim(&tenant) else {
                failed.push(why(&tenant, "another operation is in progress"));
                continue;
            };
            let copies: Vec<String> = holding
                .iter()
                .filter(|(_, names)| names.contains(&tenant))
                .map(|(node, _)| node.clone())
                .collect();
            let placed = match self.replica_node(&primary, &copies) {
                None => Err("there is no other node to keep it".to_string()),
                Some(on) => self.attach(&tenant, &primary, &on).map(|_| on),
            };
            match placed {
                Ok(on) => done.push(format!(
                    "{{\"tenant\":{},\"replica\":{}}}",
                    quote(&tenant),
                    quote(&on)
                )),
                Err(e) => failed.push(why(&tenant, &e)),
            }
        }
        Response::json(
            if failed.is_empty() { 200 } else { 502 },
            format!(
                "{{\"replicated\":[{}],\"failed\":[{}]}}",
                done.join(","),
                failed.join(",")
            ),
        )
    }

    /// Moves every tenant of `name` to the node its writes went to: each is
    /// promoted there -- its history forks and it stops following -- and the
    /// directory points at it. One tenant at a time, because they are
    /// separate databases: there is nothing to make atomic between them, and
    /// one that will not promote leaves the others promoted and says so.
    fn failover(&self, name: &str) -> Outcome<Response> {
        if self.read_dir().node(name).is_none() {
            return Err(Fail(404, format!("no node `{name}`")));
        }
        if self.read_dir().standby(name).is_none() {
            // Tenants of its own with replicas, or others' replicas on it:
            // with neither, there is nothing to fail over to.
            let dir = self.read_dir();
            let replicated = dir
                .tenants()
                .iter()
                .any(|(t, p)| p.node == name && dir.replica(t).is_some())
                || !dir.replicas_on(name).is_empty();
            drop(dir);
            if !replicated {
                return Err(Fail(
                    409,
                    format!(
                        "node `{name}` has no standby, and none of its tenants a replica: \
                         PUT /_shard/nodes/{name} {{standby}}, or POST /_shard/replicas"
                    ),
                ));
            }
            return Ok(self.failover_replicas(name));
        }
        let (to, standby) = {
            let dir = self.read_dir();
            let s = dir
                .standby(name)
                .ok_or_else(|| Fail(409, format!("node `{name}` has no standby")))?
                .to_string();
            let n = dir
                .node(&s)
                .cloned()
                .ok_or_else(|| Fail(404, format!("no node `{s}`")))?;
            (s, n)
        };
        let tenants: Vec<String> = self
            .read_dir()
            .tenants()
            .into_iter()
            .filter(|(_, p)| p.node == name)
            .map(|(t, _)| t)
            .collect();
        let (mut promoted, mut failed) = (Vec::new(), Vec::new());
        for tenant in tenants {
            let Ok(_claim) = self.claim(&tenant) else {
                failed.push(format!(
                    "{{\"tenant\":{},\"why\":\"another operation is in progress\"}}",
                    quote(&tenant)
                ));
                continue;
            };
            let target = format!("/_admin/tenants/{tenant}/promote");
            let answer = self
                .pool
                .call(&standby.addr, "POST", &target, &standby.token, b"");
            match answer {
                Ok((200, _)) => match self.write_dir().place(&tenant, &to, State::Active) {
                    Ok(()) => promoted.push(quote(&tenant)),
                    Err(e) => failed.push(format!(
                        "{{\"tenant\":{},\"why\":{}}}",
                        quote(&tenant),
                        quote(&message(&e))
                    )),
                },
                Ok((status, body)) => failed.push(format!(
                    "{{\"tenant\":{},\"why\":{}}}",
                    quote(&tenant),
                    quote(&format!(
                        "{status}: {}",
                        String::from_utf8_lossy(&body).trim()
                    ))
                )),
                Err(e) => failed.push(format!(
                    "{{\"tenant\":{},\"why\":{}}}",
                    quote(&tenant),
                    quote(&format!("`{to}` did not answer: {e}"))
                )),
            }
        }
        // Every tenant moved, the pair is over: `to` is a primary now, and
        // left recorded as `name`'s standby it would take no tenant. `name`
        // rejoins as the standby of `to`. With a tenant left behind the pair
        // stays, so a second failover can take it.
        if failed.is_empty() {
            if let Err(e) = self.write_dir().set_standby(name, None) {
                failed.push(format!(
                    "{{\"tenant\":null,\"why\":{}}}",
                    quote(&format!("the pair stayed recorded: {}", message(&e)))
                ));
            }
        }
        let body = format!(
            "{{\"node\":{},\"to\":{},\"promoted\":[{}],\"failed\":[{}]}}",
            quote(name),
            quote(&to),
            promoted.join(","),
            failed.join(",")
        );
        Ok(Response::json(
            if failed.is_empty() { 200 } else { 502 },
            body,
        ))
    }

    /// The failover of a node in no pair: each of its tenants promoted on
    /// its own replica and routed there, so its tenants spread over the
    /// nodes that held their replicas. One at a time, as a pair's are. The
    /// tenants whose replica it held have none now, and are named: a repair
    /// gives them one.
    fn failover_replicas(&self, name: &str) -> Response {
        let (tenants, lost) = {
            let dir = self.read_dir();
            let tenants: Vec<(String, Option<String>)> = dir
                .tenants()
                .into_iter()
                .filter(|(_, p)| p.node == name)
                .map(|(t, _)| {
                    let r = dir.replica(&t).map(str::to_string);
                    (t, r)
                })
                .collect();
            (tenants, dir.replicas_on(name))
        };
        let (mut promoted, mut failed) = (Vec::new(), Vec::new());
        for (tenant, replica) in tenants {
            let Ok(_claim) = self.claim(&tenant) else {
                failed.push(why(&tenant, "another operation is in progress"));
                continue;
            };
            let Some(to) = replica else {
                failed.push(why(&tenant, "it has no replica"));
                continue;
            };
            let Some(node) = self.read_dir().node(&to).cloned() else {
                failed.push(why(&tenant, &format!("no node `{to}`")));
                continue;
            };
            let target = format!("/_admin/tenants/{tenant}/promote");
            match self
                .pool
                .call(&node.addr, "POST", &target, &node.token, b"")
            {
                Ok((200, _)) => {
                    let mut dir = self.write_dir();
                    match dir
                        .place(&tenant, &to, State::Active)
                        .and_then(|_| dir.set_replica(&tenant, None))
                    {
                        Ok(()) => promoted.push(format!(
                            "{{\"tenant\":{},\"to\":{}}}",
                            quote(&tenant),
                            quote(&to)
                        )),
                        Err(e) => failed.push(why(&tenant, &message(&e))),
                    }
                }
                Ok((status, body)) => failed.push(why(
                    &tenant,
                    &format!("{status}: {}", String::from_utf8_lossy(&body).trim()),
                )),
                Err(e) => failed.push(why(&tenant, &format!("`{to}` did not answer: {e}"))),
            }
        }
        let mut unreplicated = Vec::new();
        for tenant in lost {
            match self.write_dir().set_replica(&tenant, None) {
                Ok(()) => unreplicated.push(quote(&tenant)),
                Err(e) => failed.push(why(&tenant, &message(&e))),
            }
        }
        Response::json(
            if failed.is_empty() { 200 } else { 502 },
            format!(
                "{{\"node\":{},\"promoted\":[{}],\"failed\":[{}],\"unreplicated\":[{}]}}",
                quote(name),
                promoted.join(","),
                failed.join(","),
                unreplicated.join(",")
            ),
        )
    }

    fn remove_node(&self, name: &str) -> Outcome<Response> {
        let mut dir = self.write_dir();
        if dir.node(name).is_none() {
            return Err(Fail(404, format!("no node `{name}`")));
        }
        dir.remove_node(name).map_err(|e| Fail(409, message(&e)))?;
        if let Some((leases, _)) = &self.leases {
            leases.forget(name);
        }
        Ok(Response::empty(204))
    }

    /// Marks a tenant busy for the length of an operation.
    fn claim(&self, tenant: &str) -> Outcome<Claim<'_>> {
        let mut busy = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        if !busy.insert(tenant.to_string()) {
            return Err(Fail(
                409,
                format!("another operation on `{tenant}` is in progress"),
            ));
        }
        Ok(Claim {
            router: self,
            tenant: tenant.to_string(),
        })
    }

    /// The node with the fewest bytes on disk among those that answer.
    /// Disk rather than memory: memory counts only the open tenants, and
    /// which ones are open is a matter of who asked last.
    fn least_loaded(&self) -> Outcome<String> {
        // A standby holds its primary's tenants as replicas: one created
        // there would follow a tenant its primary does not have, and refuse
        // every write. With no disk of its own yet it looked the least
        // loaded, and took every tenant nobody placed.
        let nodes: Vec<(String, Node)> = {
            let dir = self.read_dir();
            dir.nodes()
                .iter()
                .filter(|(k, _)| !dir.is_standby(k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        };
        if nodes.is_empty() {
            return Err(Fail(
                409,
                "no nodes: add one with PUT /_shard/nodes/<name>".into(),
            ));
        }
        let mut best: Option<(u64, String)> = None;
        for (name, node) in nodes {
            let Ok((200, body)) =
                self.pool
                    .call(&node.addr, "GET", "/_admin/stats", &node.token, b"")
            else {
                continue;
            };
            let disk = fenec_core::json::parse_object(&String::from_utf8_lossy(&body))
                .ok()
                .and_then(|f| {
                    f.into_iter().find_map(|(k, v)| match (k.as_str(), v) {
                        ("disk", Value::Int(n)) => Some(n.max(0) as u64),
                        _ => None,
                    })
                });
            let Some(disk) = disk else { continue };
            if best.as_ref().is_none_or(|(d, _)| disk < *d) {
                best = Some((disk, name));
            }
        }
        best.map(|(_, n)| n)
            .ok_or_else(|| Fail(503, "no node answered".into()))
    }

    fn node(&self, name: &str) -> Outcome<Node> {
        self.read_dir()
            .node(name)
            .cloned()
            .ok_or_else(|| Fail(404, format!("no node `{name}`")))
    }

    fn create(&self, tenant: &str, node: Option<&str>) -> Outcome<Response> {
        fenec_http::tenants::check_name(tenant).map_err(|r| Fail(r.0, r.1))?;
        let _claim = self.claim(tenant)?;
        if self.read_dir().placement(tenant).is_some() {
            return Err(Fail(409, format!("tenant `{tenant}` already exists")));
        }
        let name = match node {
            Some(n) => n.to_string(),
            None => self.least_loaded()?,
        };
        let n = self.node(&name)?;
        let target = format!("/_admin/tenants/{tenant}");
        expect(
            self.pool.call(&n.addr, "PUT", &target, &n.token, b""),
            201,
            &name,
        )?;
        if let Err(e) = self.write_dir().place(tenant, &name, State::Active) {
            // Not recorded means not reachable: take the file back off the
            // node rather than leave an orphan there.
            let _ = self.pool.call(&n.addr, "DELETE", &target, &n.token, b"");
            return Err(internal(e));
        }
        self.lease_now(&name);
        // The replica follows into a file of its own, so the tenant is
        // created on the standby as well.
        let warning = self.on_standby(&name, "PUT", &target);
        if let Some(w) = &warning {
            fenec_http::log!("tenant `{tenant}` has no replica yet: {w}");
        }
        let own = self.replicate(tenant, &name);
        if let Some(Err(w)) = &own {
            fenec_http::log!("tenant `{tenant}` has no replica yet: {w}");
        }
        Ok(Response::json(
            201,
            format!(
                "{{\"tenant\":{},\"node\":{}{}{}}}",
                quote(tenant),
                quote(&name),
                match &warning {
                    None => String::new(),
                    Some(w) => format!(",\"replica\":{}", quote(w)),
                },
                replica_field(&own)
            ),
        ))
    }

    fn delete(&self, tenant: &str) -> Outcome<Response> {
        let _claim = self.claim(tenant)?;
        let placement = self
            .read_dir()
            .placement(tenant)
            .cloned()
            .ok_or_else(|| Fail(404, format!("no tenant `{tenant}` in the directory")))?;
        let n = self.node(&placement.node)?;
        let replica = self.read_dir().replica(tenant).map(str::to_string);
        let (status, body) = self
            .pool
            .call(
                &n.addr,
                "DELETE",
                &format!("/_admin/tenants/{tenant}"),
                &n.token,
                b"",
            )
            .map_err(|e| {
                Fail(
                    502,
                    format!("node `{}` did not answer: {e}", placement.node),
                )
            })?;
        // 404 from the node: already gone there, which is what was asked.
        if status != 204 && status != 404 {
            return Err(Fail(status, String::from_utf8_lossy(&body).into_owned()));
        }
        self.write_dir().remove_tenant(tenant).map_err(internal)?;
        if let Some(w) = self.on_standby(
            &placement.node,
            "DELETE",
            &format!("/_admin/tenants/{tenant}"),
        ) {
            fenec_http::log!("tenant `{tenant}`'s replica was not removed: {w}");
        }
        // Its own replica: the record went with the tenant, the file here.
        if let Some(r) = replica.and_then(|r| self.read_dir().node(&r).cloned()) {
            let target = format!("/_admin/tenants/{tenant}");
            if !matches!(
                self.pool.call(&r.addr, "DELETE", &target, &r.token, b""),
                Ok((204 | 404, _))
            ) {
                fenec_http::log!("tenant `{tenant}`'s replica on {} stayed", r.addr);
            }
        }
        Ok(Response::empty(204))
    }

    /// Moves a tenant: freeze on the source, copy the image, install it on
    /// the target, flip the directory, delete the source copy.
    ///
    /// Reads keep being served by the source the whole time -- the directory
    /// still points there until the flip -- and writes get 503 with
    /// `Retry-After` from the frozen source. Nothing is refused by the
    /// router itself, so there is no window where the tenant is nowhere.
    fn relocate(&self, tenant: &str, to: &str) -> Outcome<Response> {
        let _claim = self.claim(tenant)?;
        let started = Instant::now();
        let from = self
            .read_dir()
            .placement(tenant)
            .map(|p| p.node.clone())
            .ok_or_else(|| Fail(404, format!("no tenant `{tenant}` in the directory")))?;
        if from == to {
            return Err(Fail(409, format!("tenant `{tenant}` is already on `{to}`")));
        }
        // Onto a standby -- the source's own included, which is what a fail
        // back after a failover looks like -- the tenant would open there as
        // a replica, and the source's standby copy is deleted on the way:
        // the tenant was then on no node at all, and the move said
        // "source_removed". A node takes tenants once it is nobody's standby.
        if self.read_dir().is_standby(to) {
            return Err(Fail(
                409,
                format!(
                    "`{to}` is a standby: its tenants follow another node's. Record it \
                     without one (PUT /_shard/nodes/<node> without \"standby\") to move \
                     tenants onto it"
                ),
            ));
        }
        let src = self.node(&from)?;
        let dst = self.node(to)?;
        let base = format!("/_admin/tenants/{tenant}");
        // A replica on the target goes first: the tenant arrives there as its
        // primary, and a node holds one copy of a tenant. It gets another
        // replica once it has moved.
        let mut replica = self.read_dir().replica(tenant).map(str::to_string);
        if replica.as_deref() == Some(to) {
            if let Some(w) = self.detach(tenant) {
                fenec_http::log!("tenant `{tenant}`'s replica on `{to}` stayed: {w}");
            }
            replica = None;
        }

        self.write_dir()
            .place(tenant, &from, State::Moving)
            .map_err(internal)?;
        let thaw = |why: Fail| -> Fail {
            let _ = self
                .pool
                .call(&src.addr, "POST", &format!("{base}/thaw"), &src.token, b"");
            let _ = self.write_dir().place(tenant, &from, State::Active);
            metrics::moved(false, started.elapsed());
            why
        };

        let freeze = self.pool.call(
            &src.addr,
            "POST",
            &format!("{base}/freeze"),
            &src.token,
            b"",
        );
        expect(freeze, 200, &from).map_err(thaw)?;
        let image = expect(
            self.pool
                .call(&src.addr, "GET", &format!("{base}/file"), &src.token, b""),
            200,
            &from,
        )
        .map_err(thaw)?;

        let put = |image: &[u8]| {
            self.pool
                .call(&dst.addr, "PUT", &format!("{base}/file"), &dst.token, image)
        };
        let mut installed = put(&image);
        // 409: a copy left on the target by a move that died before the
        // flip. The directory still says `from`, so that copy is stale.
        if matches!(installed, Ok((409, _))) {
            let _ = self.pool.call(&dst.addr, "DELETE", &base, &dst.token, b"");
            installed = put(&image);
        }
        expect(installed, 201, to).map_err(thaw)?;

        if let Err(e) = self.write_dir().place(tenant, to, State::Active) {
            let _ = self.pool.call(&dst.addr, "DELETE", &base, &dst.token, b"");
            return Err(thaw(internal(e)));
        }
        self.lease_now(to);

        // The replica follows the node the tenant is on now: a file there,
        // and the copy on the old node's standby is not this tenant's any
        // more.
        if let Some(w) = self.on_standby(to, "PUT", &base) {
            fenec_http::log!("tenant `{tenant}` has no replica on `{to}` yet: {w}");
        }
        if let Some(w) = self.on_standby(&from, "DELETE", &base) {
            fenec_http::log!("tenant `{tenant}`'s replica on `{from}`'s standby stayed: {w}");
        }

        // Its replica follows it there: the image carried the history, so
        // the copy goes on from where it stood. One that will not is taken
        // off, and the tenant given another.
        let own = match replica {
            Some(r) => match self.attach(tenant, to, &r) {
                Ok(()) => Some(Ok(r)),
                Err(w) => {
                    fenec_http::log!("tenant `{tenant}`'s replica on `{r}` did not follow it: {w}");
                    let _ = self.detach(tenant);
                    self.replicate(tenant, to)
                }
            },
            None => self.replicate(tenant, to),
        };

        // From here the tenant is on `to`. The source copy ends its streams
        // as it goes, and the clients reconnect through the router.
        let cleanup = self.pool.call(&src.addr, "DELETE", &base, &src.token, b"");
        let left = !matches!(cleanup, Ok((204, _)) | Ok((404, _)));
        metrics::moved(true, started.elapsed());
        if left {
            fenec_http::log!(
                "tenant `{tenant}` moved to `{to}`, but its copy on `{from}` could not be \
                 removed; it is frozen there and no longer routed to"
            );
        }
        Ok(Response::json(
            200,
            format!(
                "{{\"tenant\":{},\"from\":{},\"to\":{},\"bytes\":{},\"ms\":{},\"source_removed\":{}{}}}",
                quote(tenant),
                quote(&from),
                quote(to),
                image.len(),
                started.elapsed().as_millis(),
                !left,
                replica_field(&own)
            ),
        ))
    }
}

struct Claim<'a> {
    router: &'a Router,
    tenant: String,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.router
            .busy
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.tenant);
    }
}

/// The answer a node gave, if it is the expected status; its body otherwise
/// becomes the error.
fn expect(got: std::io::Result<(u16, Vec<u8>)>, want: u16, node: &str) -> Outcome<Vec<u8>> {
    match got {
        Ok((status, body)) if status == want => Ok(body),
        Ok((status, body)) => Err(Fail(
            if status >= 500 { 502 } else { status },
            format!("node `{node}`: {}", String::from_utf8_lossy(&body)),
        )),
        Err(e) => Err(Fail(502, format!("node `{node}` did not answer: {e}"))),
    }
}

/// `{tenant, why}`, a failure's entry in a list of them.
fn why(tenant: &str, why: &str) -> String {
    format!("{{\"tenant\":{},\"why\":{}}}", quote(tenant), quote(why))
}

/// What an answer says of a tenant's own replica: where it went, or why it
/// went nowhere.
fn replica_field(own: &Option<std::result::Result<String, String>>) -> String {
    match own {
        None => String::new(),
        Some(Ok(on)) => format!(",\"replica_on\":{}", quote(on)),
        Some(Err(w)) => format!(",\"unreplicated\":{}", quote(w)),
    }
}

/// The names in a node's `GET /_admin/tenants`, a JSON array of strings.
fn names_in(body: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(body);
    text.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|n| n.trim().trim_matches('"').to_string())
        .filter(|n| !n.is_empty())
        .collect()
}

/// Headers that describe one connection, not the message: never forwarded.
fn hop_by_hop(name: &str) -> bool {
    [
        "connection",
        "keep-alive",
        "transfer-encoding",
        "te",
        "trailer",
        "upgrade",
        "proxy-authorization",
        "proxy-authenticate",
        "content-length",
    ]
    .iter()
    .any(|h| name.eq_ignore_ascii_case(h))
}

fn body_fields(req: &Request) -> std::result::Result<Vec<(String, Value)>, String> {
    let text = std::str::from_utf8(&req.body).map_err(|_| "the body is not UTF-8".to_string())?;
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    fenec_core::json::parse_object(text).map_err(|e| message(&e))
}

fn field<'a>(body: &'a [(String, Value)], name: &str) -> Option<&'a str> {
    body.iter().find_map(|(k, v)| match v {
        Value::Text(s) if k == name => Some(s.as_str()),
        _ => None,
    })
}

fn quote(s: &str) -> String {
    let mut out = String::new();
    fenec_core::json::escape_into(&mut out, s);
    out
}

fn message(e: &Error) -> String {
    match e {
        Error::NotFound(m)
        | Error::Exists(m)
        | Error::Type(m)
        | Error::Query(m)
        | Error::Corrupt(m)
        | Error::Io(m)
        | Error::Plugin(m)
        | Error::ReadOnly(m)
        | Error::Denied(m) => m.clone(),
    }
}

fn internal(e: Error) -> Fail {
    Fail(500, format!("directory: {}", message(&e)))
}

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
