//! # fenec-shard
//!
//! A router in front of several `fenec-server --dir` nodes. Each tenant lives
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
pub mod fanout;
pub mod lease;
pub mod metrics;
pub mod region;
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
    /// fenec studio at `/_studio/` (`--studio`): the same pages a node
    /// serves, on the router's origin, so the tenants they reach through it
    /// are this origin's -- a page a node served would be another origin's
    /// to the router, and its tokens' preflights refused there.
    pub studio: Option<Arc<fenec_http::studio::Studio>>,
    /// `Access-Control-Allow-Origin` (`--http-cors`, `*` or one origin),
    /// read as a node reads its own ([`fenec_http::cors`]). On every answer
    /// the router gives, its own and the ones it forwards, whose node's
    /// CORS headers it replaces; `None` leaves those as the node wrote them
    /// and sends none of its own.
    pub cors: Option<String>,
    /// What a query across tenants (`POST /_shard/query`) may gather, how
    /// long it may take and how many calls it has out at once
    /// (`--fanout-*`).
    pub fanout: fanout::Limits,
    /// The region a tenant created without a `region` or a `node` is placed
    /// in (`--default-region`). `None`: on the node with the fewest bytes
    /// whatever its region, as before regions, and the tenant is in that
    /// node's region.
    pub default_region: Option<String>,
    /// The region whose own directory this router keeps (`--region`): its
    /// nodes and tenants, leased and failed over here with no word from
    /// another region ([`region`]). `None`: the global directory's router.
    pub region: Option<String>,
    /// The global directory's routers (`--global`), the one keeping it
    /// first: where a region's router sends a change the global directory
    /// takes -- a create, a delete, a move across regions.
    pub global: Vec<String>,
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
            studio: None,
            cors: None,
            fanout: fanout::Limits::default(),
            default_region: None,
            region: None,
            global: Vec::new(),
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
    /// Each node's mark for what is forwarded to it
    /// (`fenec_http::audit::router_mark`), by its admin token: an HMAC
    /// worked out once, not a request.
    marks: Mutex<std::collections::HashMap<String, Arc<str>>>,
    /// A region's router's copy of the global directory, which says where
    /// every other tenant lives: followed from the global router. `None` on
    /// a router of the global directory, whose own it is.
    global: Option<RwLock<Directory>>,
    /// The mark a router names a relayed request's client with, and
    /// believes on what it is relayed: an HMAC of the routers' `--token`,
    /// which they share. `None` without one: the relaying router is then
    /// the client.
    relay_mark: Option<Arc<str>>,
    /// For each region, the router of its list that answered last: asked
    /// first, so a router gone costs its region one failed connect, not one
    /// a request.
    answered: Mutex<std::collections::HashMap<String, usize>>,
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
        Router::assemble(dir, cfg, repl, None)
    }

    /// The router over `dir`, and over `global` where it keeps a region's
    /// own directory ([`region`]).
    fn assemble(
        dir: Directory,
        cfg: Config,
        repl: Option<Arc<Replication>>,
        global: Option<RwLock<Directory>>,
    ) -> Arc<Router> {
        Arc::new(Router {
            dir: RwLock::new(dir),
            repl,
            pool: Pool::new(cfg.upstream_timeout),
            busy: Mutex::new(HashSet::new()),
            marks: Mutex::new(Default::default()),
            live: AtomicUsize::new(0),
            leases: cfg
                .auto_failover
                .map(|term| (lease::Leases::new(term), Pool::new(term / 3))),
            global,
            relay_mark: cfg
                .token
                .as_deref()
                .map(|t| fenec_http::audit::router_mark(t).into()),
            answered: Mutex::new(Default::default()),
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
                fenec_http::request_id::begin(None);
                let _ = self
                    .cors(Response::retry_after(503, "too many connections", 1))
                    .write(&mut s, false, false);
                fenec_http::request_id::end();
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
        let (lists, reading, holder) = self.primaries_read(leases);
        std::thread::scope(|s| {
            for (node, list) in &lists {
                leases.know(node);
                let holder = &holder;
                s.spawn(move || self.grant(node, list, reading, holder));
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
            let (lists, reading, holder) = self.primaries_read(leases);
            std::thread::scope(|s| {
                for (node, list) in &lists {
                    let holder = &holder;
                    s.spawn(move || self.grant(node, list, reading, holder));
                }
            });
        }
    }

    /// Lease grants waiting for their node's turn behind another: how a
    /// test knows a create's grant is queued behind a round's it holds.
    #[doc(hidden)]
    pub fn grants_waiting(&self) -> usize {
        self.leases.as_ref().map_or(0, |(l, _)| l.grants_waiting())
    }

    /// Every node's primaries, the number of this reading of the directory,
    /// taken under its lock ([`lease::Leases::reading`]), and who the grants
    /// over them come from: the directory and its term as they stood then.
    fn primaries_read(
        &self,
        leases: &lease::Leases,
    ) -> (
        std::collections::BTreeMap<String, Vec<String>>,
        u64,
        fenec_http::lease::Holder,
    ) {
        let dir = self.read_dir();
        (dir.primaries(), leases.reading(), self.holder_of(&dir))
    }

    /// Who this router leases as: the directory it keeps -- the global one,
    /// or its region's -- and the term its history stands at, which a
    /// promotion moves on.
    fn holder_of(&self, dir: &Directory) -> fenec_http::lease::Holder {
        let db = dir.db().read().unwrap_or_else(|e| e.into_inner());
        let history = db.history();
        fenec_http::lease::Holder {
            authority: self.cfg.region.clone().unwrap_or_default(),
            term: history.lineage.len() as u64,
            history: history.current(),
        }
    }

    /// [`Self::holder_of`] as the header a promotion or a follow names it
    /// in ([`fenec_http::lease::HOLDER_HEADER`]).
    fn holder_header(&self) -> String {
        self.holder_of(&self.read_dir()).header()
    }

    /// Grants `node` its lease at once, over what the directory places there
    /// now: after a create or a move, so a tenant takes writes on the node it
    /// was placed on before the next round.
    fn lease_now(&self, node: &str) {
        let Some((leases, _)) = &self.leases else {
            return;
        };
        let (mut lists, reading, holder) = self.primaries_read(leases);
        let list = lists.remove(node).unwrap_or_default();
        self.grant(node, &list, reading, &holder);
    }

    /// Sends `node` its lease over `primaries`, read from the directory's
    /// `reading`: the list itself only when the node may not hold it, and
    /// again with it when the node says it does not. One grant to a node at
    /// a time, and none over a reading older than the last one sent, which
    /// the node then holds the list of ([`lease::Leases::turn`]). A node
    /// that answers after it was failed over has a repair run, which has its
    /// copies follow its tenants' new primaries.
    fn grant(
        &self,
        node: &str,
        primaries: &[String],
        reading: u64,
        holder: &fenec_http::lease::Holder,
    ) {
        let Some((leases, pool)) = &self.leases else {
            return;
        };
        let Some(n) = self.read_dir().node(node).cloned() else {
            return;
        };
        let turn = leases.turn(node);
        let mut sent = leases.wait_turn(&turn);
        if reading < *sent {
            return;
        }
        *sent = reading;
        let epoch = lease::epoch_of(primaries);
        let mut with_list = leases.epoch(node).as_deref() != Some(epoch.as_str());
        let mut back = false;
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
            body.push_str(&format!(
                ",\"authority\":{},\"term\":{},\"history\":\"{:016x}\"}}",
                quote(&holder.authority),
                holder.term,
                holder.history
            ));
            let answer =
                match pool.call(&n.addr, "POST", "/_admin/lease", &n.token, body.as_bytes()) {
                    // A node from before terms answers without one. Under the
                    // global directory it is leased as before; a region's
                    // router, which another router may be promoted over,
                    // does not count on a fence the node does not have.
                    Ok((200, b))
                        if !holder.authority.is_empty()
                            && !b.windows(7).any(|w| w == b"\"term\":") =>
                    {
                        lease::Answer::Unfenced
                    }
                    Ok((200, _)) => lease::Answer::Taken,
                    Ok((412, _)) => lease::Answer::NeedsList,
                    Ok((409, b)) if b.windows(9).any(|w| w == b"\"held_by\"") => {
                        lease::Answer::Superseded
                    }
                    Ok((404 | 409, _)) => lease::Answer::Refuses,
                    _ => lease::Answer::Silent,
                };
            back |= leases.answered(node, &epoch, &answer, Instant::now());
            match answer {
                lease::Answer::NeedsList if !with_list => with_list = true,
                _ => break,
            }
        }
        // The repair calls on nodes for a while: no grant to this one waits
        // for it.
        drop(sent);
        if back {
            fenec_http::log!(
                "node `{node}` answers again: its copies are to follow the tenants' new primaries"
            );
            let r = self.repair();
            fenec_http::log!("repair: {} {}", r.status, String::from_utf8_lossy(&r.body));
        }
    }

    /// The mark of what is forwarded to the node whose admin token is
    /// `token`.
    fn mark(&self, token: &str) -> Arc<str> {
        let mut marks = self.marks.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(m) = marks.get(token) {
            return Arc::clone(m);
        }
        let m: Arc<str> = fenec_http::audit::router_mark(token).into();
        marks.insert(token.to_string(), Arc::clone(&m));
        m
    }

    /// `resp` with the CORS headers `--http-cors` asks for: on every answer
    /// the router gives itself. Without them a page on another origin read
    /// a standby's 503, an unknown tenant's 404 or a node's 502 as the
    /// network failing -- the sync layer and `FenecHttp.live` backed off
    /// from what was an answer -- while the node's answers through the
    /// router carried the node's.
    fn cors(&self, resp: Response) -> Response {
        fenec_http::cors::apply(resp, self.cfg.cors.as_deref())
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
        let peer = stream.peer_addr().ok();
        fenec_http::audit::connection("http", peer);
        fenec_http::trace::connection(stream.local_addr().ok(), peer);
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
                    fenec_http::request_id::begin(None);
                    let _ = self
                        .cors(Response::error(status, &msg))
                        .write(&mut out, false, false);
                    return;
                }
            };
            // The client's id, or one made here, which the node is sent
            // (`Pool::send`) and answers with.
            fenec_http::request_id::begin_request(&req);
            let arrived = Instant::now();
            // The client another router relayed this for, believed from a
            // router holding the same `--token` alone ([`region`]): it
            // waited out a refusal by that client's address already, and is
            // told when a token is verified here.
            let relayed = fenec_http::audit::request(&req, self.relay_mark.as_deref());
            // A browser's preflight is answered here, as a node answers its
            // own: before the directory is read, so a tenant the directory
            // does not hold, a standby whose directory has not arrived or a
            // node that cannot be reached still lets the page send its
            // request and read the answer -- forwarded, the preflight to
            // any of them failed, and the page saw nothing but a network
            // error. It reads nothing and says only what the headers on
            // every answer say. No refusal count is touched: it verifies no
            // token, so a preflight carrying `Authorization` clears nothing
            // (`audit::verified`), and it is refused for none.
            if req.method == Method::Options
                && fenec_http::cors::allows(self.cfg.cors.as_deref(), &req)
            {
                let sent = self
                    .cors(Response::empty(204))
                    .write(&mut out, req.keep_alive, false)
                    .is_ok();
                metrics::request(route_of(&req), 204, arrived.elapsed());
                if !(sent && req.keep_alive) {
                    return;
                }
                continue;
            }
            // Everything but a forwarded request is counted here, as it is
            // answered; `forward` counts its own, a stream at its head.
            let answer = |out: &mut TcpStream, route: Route, resp: Response| {
                fenec_http::audit::http(&req, resp.status, peer);
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
            if let Some(global) = &self.global {
                if read(global).stale() {
                    if let Err(e) = write(global).refresh() {
                        fenec_http::log!("could not read the global directory: {e}");
                    }
                }
            }
            let keep = match req.segments().first() {
                Some(&"t") => self.forward(&req, &mut out, arrived, peer, relayed),
                Some(&"_replication") => {
                    let Some(repl) = self.repl.clone() else {
                        let _ = self
                            .cors(Response::error(
                                404,
                                "this router has no --replication-token",
                            ))
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
                        Some(resp) => answer(&mut out, Route::Replication, self.cors(resp)),
                    }
                }
                // A query across tenants is traced as a forward is: its
                // calls to the nodes are its spans, and the nodes' hang
                // under them. The rest of `/_shard/` is not.
                Some(&"_shard") if fanout::asked(&req) => {
                    fenec_http::trace::begin(&req);
                    let resp = self.cors(self.admin(&req));
                    let status = resp.status;
                    let keep = answer(&mut out, Route::Shard, resp);
                    fenec_http::trace::end(status);
                    keep
                }
                Some(&"_shard") => answer(&mut out, Route::Shard, self.cors(self.admin(&req))),
                // The studio's files go without, as a node's do: pages
                // opened, never fetched from another origin.
                Some(&"_studio") => answer(
                    &mut out,
                    Route::Other,
                    fenec_http::studio::handle(self.cfg.studio.as_deref(), &req),
                ),
                Some(&"_metrics") => {
                    answer(&mut out, Route::Metrics, self.cors(self.metrics(&req)))
                }
                _ => answer(
                    &mut out,
                    Route::Other,
                    self.cors(Response::error(
                        404,
                        "the router serves /t/<tenant>/..., /_shard/ and /_metrics",
                    )),
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
    /// `relayed` is the client another router relayed it for, where it
    /// came so ([`region`]).
    fn forward(
        &self,
        req: &Request,
        out: &mut TcpStream,
        arrived: Instant,
        peer: Option<std::net::SocketAddr>,
        relayed: Option<std::net::IpAddr>,
    ) -> bool {
        let head_only = req.method == Method::Head;
        // A forwarded request is traced, the rest of the router's are not:
        // a scrape or a lease's grant would be most of the spans.
        fenec_http::trace::begin(req);
        // The router's own answers: a standby's 503, an unknown tenant's
        // 404, a node's 502.
        let reply = |out: &mut TcpStream, resp: Response| {
            let resp = self.cors(resp);
            let sent = resp.write(out, req.keep_alive, head_only).is_ok();
            metrics::request(Route::Tenant, resp.status, arrived.elapsed());
            fenec_http::trace::end(resp.status);
            sent && req.keep_alive
        };
        let segs = req.segments();
        let tenant = segs.get(1).copied().unwrap_or("");
        if fenec_http::trace::recording() {
            fenec_http::trace::attr("fenec.tenant", tenant.to_string());
        }
        // The node holding the tenant, as this router's own directory has
        // it; or, where that says nothing, the routers of the region the
        // tenant lives in ([`region`]).
        let placed = {
            let dir = self.read_dir();
            dir.placement(tenant).and_then(|p| {
                dir.node(&p.node)
                    .map(|n| (p.node.clone(), n.addr.clone(), self.mark(&n.token)))
            })
        };
        let (node, addr, mark, routers) = match placed {
            Some((node, addr, mark)) => (node, addr, Some(mark), None),
            None => match self.elsewhere(tenant, relayed.is_some()) {
                region::Target::Node(node, addr, mark) => (node, addr, Some(mark), None),
                region::Target::Region(region, routers) => {
                    let first = self.first_router(&region, &routers);
                    let addr = region::host(&routers[first]).to_string();
                    let mark = self.relay_mark.clone();
                    (region::label(&region), addr, mark, Some((region, routers)))
                }
                region::Target::Answer(resp) => return reply(out, resp),
            },
        };
        if fenec_http::trace::recording() {
            fenec_http::trace::attr("fenec.node", node.clone());
        }
        // The router's own context goes to the node in place of the
        // client's (`Pool::send`), while it traces.
        let traced = fenec_http::trace::propagating();

        // The node believes the client's address from the router alone:
        // what a client sent under this name is dropped, and the router's
        // own put in its place.
        // Borrowed, not cloned: a request's headers were copied into
        // `String`s here and formatted one by one into the head, which with
        // the router's own header made a request through it 1.5 us slower.
        // A router relaying names the client it was relayed for, as the
        // router before it did.
        let client = relayed.or(peer.map(|p| p.ip()));
        let named = client.zip(mark).map(|(ip, mark)| format!("{ip} {mark}"));
        let mut headers: Vec<(&str, &str)> = req
            .headers
            .iter()
            .filter(|(k, _)| {
                !hop_by_hop(k)
                    && !k.eq_ignore_ascii_case("host")
                    && !k.eq_ignore_ascii_case(fenec_http::audit::ROUTER_HEADER)
                    && !k.eq_ignore_ascii_case(fenec_http::request_id::HEADER)
                    && !(traced
                        && (k.eq_ignore_ascii_case(fenec_http::trace::TRACEPARENT)
                            || k.eq_ignore_ascii_case(fenec_http::trace::TRACESTATE)))
            })
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        if let Some(named) = &named {
            headers.push((fenec_http::audit::ROUTER_HEADER, named));
        }
        let sent = Instant::now();
        // The forward, to the node's whole answer or a stream's head: the
        // node's server span is its child.
        let forwarding = fenec_http::trace::span_of("forward", fenec_http::trace::Kind::Client);
        if fenec_http::trace::recording() {
            forwarding.attr("http.request.method", req.method.name());
            forwarding.attr("fenec.node", node.clone());
            match addr.rsplit_once(':') {
                Some((host, port)) => {
                    forwarding.attr("server.address", host.to_string());
                    if let Ok(p) = port.parse::<i64>() {
                        forwarding.attr("server.port", p);
                    }
                }
                None => forwarding.attr("server.address", addr.clone()),
            }
        }
        // A request the node may hold -- `Fenec-Wait`, for a claim or for
        // `Fenec-After` -- is waited for as long besides the timeout.
        let held = req
            .header("fenec-wait")
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map_or(Duration::ZERO, |ms| {
                Duration::from_millis(ms).min(fenec_http::waits::LONGEST)
            });
        let sending = match &routers {
            None => self.pool.send_held(
                &addr,
                req.method.name(),
                &req.target,
                &headers,
                &req.body,
                held,
            ),
            // A region's routers, in turn from the one that answered last,
            // as far as the request cannot have run.
            Some((region, routers)) => self.relay_send(region, routers, req, &headers, held),
        };
        let answer = match sending {
            Ok(a) => a,
            Err(e) => {
                forwarding.error(e.to_string());
                drop(forwarding);
                metrics::unreachable(&node);
                // 502, as a node out of reach is: a client takes it to ask
                // again, and a region cut off from this one answers its own
                // clients all the while.
                let why = match &routers {
                    None => format!("node `{node}` ({addr}) did not answer: {e}"),
                    Some((region, _)) => format!(
                        "region `{region}`'s routers did not answer -- it may be cut off from \
                         this one, and answers through its own: {e}"
                    ),
                };
                return reply(out, Response::error(502, &why));
            }
        };

        let status = answer.status;
        forwarding.attr("http.response.status_code", status);
        if status >= 500 {
            forwarding.error(status.to_string());
        }
        let mut head = format!("HTTP/1.1 {status} {}\r\n", http::reason(status));
        let cors = self.cfg.cors.as_deref();
        let mut verified = false;
        for (k, v) in &answer.headers {
            // The node answers with the id it was sent; the router's own
            // goes out in its place all the same, which a node from before
            // the ids leaves out. So do the router's CORS headers, where it
            // has its own: a page is told one rule, the one the router's
            // preflight told it. Whether the node verified a token is the
            // router's to know and no one else's.
            if k.eq_ignore_ascii_case(fenec_http::audit::VERIFIED_HEADER) {
                verified = true;
            } else if !hop_by_hop(k)
                && !k.eq_ignore_ascii_case("content-length")
                && !k.eq_ignore_ascii_case(fenec_http::request_id::HEADER)
                && !(cors.is_some() && fenec_http::cors::is_cors_header(k, v))
            {
                head.push_str(&format!("{k}: {v}\r\n"));
            }
        }
        if let Some(origin) = cors {
            fenec_http::cors::push_into(&mut head, origin);
        }
        // A token the node verified starts the client's count again -- by
        // its own address, no other -- and the node's word is the only
        // thing that does: a status says nothing of the token, and a node
        // answers a preflight, a body over its ceiling or a server asking
        // no token without looking at one. Then a refusal waits here, by
        // the client's address, before it is answered: the node waits none
        // for what the router forwards.
        if verified {
            fenec_http::audit::verified();
        }
        // Relayed, it is the relaying router's to know, which keeps the
        // client's count.
        fenec_http::audit::push_verified(&mut head);
        fenec_http::audit::http(req, status, peer);
        fenec_http::request_id::with(|id| {
            head.push_str(fenec_http::request_id::HEADER);
            head.push_str(": ");
            head.push_str(id);
            head.push_str("\r\n");
        });

        if let Some(length) = answer.content_length() {
            let body = match answer.read_body(&self.pool, head_only) {
                Ok(b) => b,
                Err(e) => {
                    forwarding.error(e.to_string());
                    drop(forwarding);
                    metrics::unreachable(&node);
                    return reply(
                        out,
                        Response::error(502, &format!("node `{node}` broke off: {e}")),
                    );
                }
            };
            drop(forwarding);
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
            fenec_http::trace::end(status);
            return written.is_ok() && req.keep_alive;
        }

        // No length: a stream. It is copied as it arrives until either side
        // closes, and the client connection ends with it; it is counted,
        // and timed to its head.
        drop(forwarding);
        metrics::upstream(sent.elapsed());
        head.push_str("Connection: close\r\n\r\n");
        let written = out.write_all(head.as_bytes());
        metrics::request(Route::Tenant, status, arrived.elapsed());
        fenec_http::trace::end(status);
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
    /// GET    /_shard/nodes                     [{name, addr, region, zone}]
    /// PUT    /_shard/nodes/<n>   {addr, token, region, zone}  add or change
    ///                                       a node; its region is fixed while
    ///                                       it holds a tenant or a copy
    /// DELETE /_shard/nodes/<n>                 refused while it holds tenants
    /// GET    /_shard/tenants                   [{name, node, state, region}];
    ///                                       with ?bytes, each one's bytes on
    ///                                       its node; ?region=<r>, that region's
    /// GET    /_shard/tenants/<t>               {name, node, state, region, ..}
    /// PUT    /_shard/tenants/<t> [{region, node, schema, quota}]  place and
    ///                                       create; least disk in the region
    ///                                       wins; `schema`, FenecQL text,
    ///                                       applied there and `quota` set
    ///                                       there before the tenant is answered
    /// PUT    /_shard/tenants/<t>/quota {bytes, rows, ...}  the tenant's quota
    ///                                       over its node's, kept here and sent
    ///                                       to every node holding a copy
    /// GET    /_shard/tenants/<t>/quota         the node's report: set, quota, usage
    /// DELETE /_shard/tenants/<t>               delete on the node, then forget
    /// POST   /_shard/tenants/<t>/move {to, region}  move to another node: in
    ///                                       its region, or across to the one
    ///                                       the body names
    /// PUT    /_shard/nodes/<n> {addr, token, standby}  standby: the node
    ///                                       whose --replica-of follows this
    ///                                       one, where its tenants are copied
    /// POST   /_shard/nodes/<n>/failover      promote this node's tenants on
    ///                                       its standby -- or each on its own
    ///                                       replica -- and route them there
    /// POST   /_shard/replicas                  give every tenant in no pair
    ///                                       that has no replica one
    /// POST   /_shard/query {query, params, tenants | prefix, merge, column,
    ///                       partial, timeout_ms}  one read on every tenant,
    ///                                       the answers merged ([`fanout`])
    /// GET    /_shard/region                    {region, following, term,
    ///                                       history, global}: what this router
    ///                                       keeps, and the term it leases at
    /// GET    /_shard/regions                   [{name, routers, tenants}]: the
    ///                                       regions keeping their own directory
    /// PUT    /_shard/regions/<r> {routers}    region r keeps its own, kept by
    ///                                       the first router and followed by the
    ///                                       rest; DELETE once no tenant is there
    /// ```
    ///
    /// Where regions keep their own directory ([`region`]), a change is
    /// relayed to the router keeping the directory it touches -- a region's
    /// nodes and tenants to its router, a create, a delete and a move across
    /// regions to the global router, which asks each region for its part.
    pub fn admin(&self, req: &Request) -> Response {
        if let Some(refused) = self.refused(req) {
            return refused;
        }
        // Its body is not the object of fields the others' is: what goes
        // to the nodes stays the text it came as. A read, so a standby
        // router answers it from the placements it follows.
        if req.segments().get(1) == Some(&"query") {
            return match fanout::asked(req) {
                true => self.fanout(req),
                false => Response::error(
                    405,
                    "a query across tenants is sent with POST to /_shard/query",
                ),
            };
        }
        let segs = req.segments();
        // A standby's directory is a replica's file: it takes no write of
        // its own, so a change belongs on the primary. The reads below are
        // answered from the maps the primary's writes filled.
        let writes = !matches!(
            (req.method, &segs[1..]),
            (Method::Get, ["nodes"])
                | (Method::Get, ["tenants"])
                | (Method::Get, ["tenants", _])
                | (Method::Get, ["tenants", _, "quota"])
                | (Method::Get, ["region"])
                | (Method::Get, ["regions"])
        );
        if writes && self.read_dir().following() {
            return Response::error(
                409,
                "this router follows another: send /_shard/ changes to the primary, \
                 or promote this one (POST /_replication/promote)",
            );
        }
        // What regions keeping their own directory change: relayed to the
        // router keeping the directory it touches, or carried out here
        // across two of them ([`region`]).
        if let Some(resp) = self.regional_admin(req) {
            return resp;
        }
        let body = match body_fields(req) {
            Ok(b) => b,
            Err(e) => return Response::error(400, &e),
        };
        let result = match (req.method, &segs[1..]) {
            (Method::Get, ["nodes"]) => Ok(self.list_nodes()),
            (Method::Put, ["nodes", n]) => self.set_node(n, &body),
            (Method::Delete, ["nodes", n]) => self.remove_node(n),
            (Method::Post, ["nodes", n, "failover"]) => self.failover(n),
            (Method::Post, ["replicas"]) => Ok(self.repair()),
            (Method::Get, ["tenants"]) => Ok(self.list_tenants(req)),
            (Method::Get, ["tenants", t]) => self.show_tenant(t),
            (Method::Put, ["tenants", t]) => match quota_member(&body) {
                Ok(quota) => place_field(&body, "region").and_then(|region| {
                    let at = At {
                        node: field(&body, "node"),
                        region,
                    };
                    self.create(t, at, field(&body, "schema"), quota)
                }),
                Err(e) => Err(Fail(400, e)),
            },
            (Method::Put, ["tenants", t, "quota"]) => self.set_quota(t, &req.body),
            (Method::Get, ["tenants", t, "quota"]) => self.quota_report(t),
            (Method::Delete, ["tenants", t]) => self.delete(t),
            (Method::Post, ["tenants", t, "move"]) => {
                place_field(&body, "region").and_then(|region| match field(&body, "to") {
                    None if region.is_none() => Err(Fail(
                        400,
                        "the body needs {\"to\": \"<node>\"}, {\"region\": \"<region>\"} or both"
                            .into(),
                    )),
                    to => self.relocate(t, At { node: to, region }),
                })
            }
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
        if constant_eq(given.as_bytes(), token.as_bytes()) {
            fenec_http::audit::verified();
            return None;
        }
        Some(Response::error(401, "invalid or missing token").header("WWW-Authenticate", "Bearer"))
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
                node_sample(&mut out, "fenec_router_tenants", &dir, node, n);
            }
            out.family(
                "fenec_router_replicas",
                "gauge",
                "Tenants' replicas of their own each node holds.",
            );
            for (node, n) in dir.replicas_by_node() {
                node_sample(&mut out, "fenec_router_replicas", &dir, node, n);
            }
            out.family(
                "fenec_router_same_zone_copies",
                "gauge",
                "Tenants whose replica or standby copy is in their primary's zone, by region: a datacenter lost takes both. Placed there only where the region had no node free in another zone.",
            );
            for (region, n) in dir.same_zone_copies() {
                match region {
                    "" => out.sample("fenec_router_same_zone_copies", &[], n),
                    r => out.sample("fenec_router_same_zone_copies", &[("region", r)], n),
                }
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
                    node_sample(&mut out, "fenec_router_lease_age_seconds", &dir, &node, age);
                }
                out.family(
                    "fenec_router_node_failed_over",
                    "gauge",
                    "1 for a node failed over on its own and not answering again since.",
                );
                for (node, _, lost) in leases.ages() {
                    node_sample(
                        &mut out,
                        "fenec_router_node_failed_over",
                        &dir,
                        &node,
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
            seq: None,
            version: None,
        }
    }

    fn list_nodes(&self) -> Response {
        let dir = self.read_dir();
        let items: Vec<String> = dir
            .nodes()
            .iter()
            .map(|(name, n)| {
                let mut item = format!("{{\"name\":{},\"addr\":{}", quote(name), quote(&n.addr));
                for (key, value) in [
                    ("region", n.region.as_str()),
                    ("zone", n.zone.as_str()),
                    ("standby", dir.standby(name).unwrap_or("")),
                ] {
                    if !value.is_empty() {
                        item.push_str(&format!(",\"{key}\":{}", quote(value)));
                    }
                }
                item.push('}');
                item
            })
            .collect();
        Response::json(200, format!("[{}]", items.join(",")))
    }

    /// `GET /_shard/tenants/<t>`: where the tenant lives -- its node, state
    /// and region, and the node holding its copy, a replica of its own or
    /// its node's standby.
    fn show_tenant(&self, tenant: &str) -> Outcome<Response> {
        let dir = self.read_dir();
        let p = dir
            .placement(tenant)
            .ok_or_else(|| Fail(404, format!("no tenant `{tenant}` in the directory")))?;
        let mut item = format!(
            "{{\"name\":{},\"node\":{},\"state\":\"{}\"",
            quote(tenant),
            quote(&p.node),
            if p.state == State::Moving {
                "moving"
            } else {
                "active"
            }
        );
        let zone = dir.node(&p.node).map_or("", |n| n.zone.as_str());
        for (key, value) in [
            ("region", dir.region_of(&p.node)),
            ("zone", zone),
            ("replica", dir.replica(tenant).unwrap_or("")),
            ("standby", dir.standby(&p.node).unwrap_or("")),
        ] {
            if !value.is_empty() {
                item.push_str(&format!(",\"{key}\":{}", quote(value)));
            }
        }
        if let Some(c) = dir.copy_of(tenant) {
            if dir.same_zone(&p.node, c) {
                item.push_str(",\"same_zone\":true");
            }
        }
        item.push('}');
        Ok(Response::json(200, item))
    }

    /// The directory's tenants; with `?bytes`, each with the bytes it holds
    /// on its node (`/_admin/sizes`, asked once a node with no lock held),
    /// which the studio's list of tenants shows. A node that does not
    /// answer leaves its tenants without a size rather than failing the list.
    fn list_tenants(&self, req: &Request) -> Response {
        let sized = req.query.iter().any(|(k, _)| k == "bytes");
        let quotas = req.query.iter().any(|(k, _)| k == "quota");
        // `?region=<r>`: that region's tenants alone; `?region=` the unnamed
        // region's.
        let region = req
            .query
            .iter()
            .find(|(k, _)| k == "region")
            .map(|(_, v)| v.as_str());
        let (tenants, nodes) = {
            let dir = self.read_dir();
            let mut tenants: Vec<(String, String, bool, Option<String>, String)> = dir
                .tenants()
                .iter()
                .filter(|(_, p)| region.is_none_or(|r| dir.region_of(&p.node) == r))
                .map(|(name, p)| {
                    (
                        name.clone(),
                        p.node.clone(),
                        p.state == State::Moving,
                        dir.replica(name).map(str::to_string),
                        dir.region_of(&p.node).to_string(),
                    )
                })
                .collect();
            // The tenants at home in a region keeping its own directory,
            // with no node: their region's router places them, and answers
            // `GET /_shard/tenants/<t>` for them.
            let homes = dir.homes();
            if !homes.is_empty() {
                tenants.extend(
                    homes
                        .into_iter()
                        .filter(|(_, r)| region.is_none_or(|want| want == r))
                        .map(|(name, r)| (name, String::new(), false, None, r)),
                );
                tenants.sort_by(|a, b| a.0.cmp(&b.0));
            }
            let nodes: Vec<(String, Node)> = match sized || quotas {
                true => dir
                    .nodes()
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                false => Vec::new(),
            };
            (tenants, nodes)
        };
        let mut sizes: Vec<(String, String, i64)> = Vec::new();
        // `?quota`: each tenant's quota and what it uses of it, as its node
        // reports them (`/_admin/quotas`), which the studio shows beside
        // its size.
        let mut used: Vec<(String, String, String)> = Vec::new();
        for (node, n) in nodes.iter().filter(|_| quotas) {
            let Ok((200, body)) = self
                .pool
                .call(&n.addr, "GET", "/_admin/quotas", &n.token, b"")
            else {
                continue;
            };
            let Ok(reports) = fenec_core::json::parse_documents(&String::from_utf8_lossy(&body))
            else {
                continue;
            };
            for r in reports {
                let get = |k: &str| r.iter().find(|(f, _)| f == k).map(|(_, v)| v);
                let (Some(Value::Text(t)), Some(q), Some(u)) =
                    (get("tenant"), get("quota"), get("usage"))
                else {
                    continue;
                };
                let mut out = String::from(",\"quota\":");
                fenec_core::json::value_into(&mut out, q);
                out.push_str(",\"usage\":");
                fenec_core::json::value_into(&mut out, u);
                used.push((node.clone(), t.clone(), out));
            }
        }
        for (node, n) in nodes.iter().filter(|_| sized) {
            let Ok((200, body)) = self
                .pool
                .call(&n.addr, "GET", "/_admin/sizes", &n.token, b"")
            else {
                continue;
            };
            let Ok(fields) = fenec_core::json::parse_object(&String::from_utf8_lossy(&body)) else {
                continue;
            };
            for (t, v) in fields {
                if let Value::Int(b) = v {
                    sizes.push((node.clone(), t, b));
                }
            }
        }
        let items: Vec<String> = tenants
            .iter()
            .map(|(name, node, moving, replica, region)| {
                let region = match region.as_str() {
                    "" => String::new(),
                    r => format!(",\"region\":{}", quote(r)),
                };
                let replica = match replica {
                    Some(r) => format!(",\"replica\":{}", quote(r)),
                    None => String::new(),
                };
                let bytes = sizes
                    .iter()
                    .find(|(n, t, _)| n == node && t == name)
                    .map(|(_, _, b)| format!(",\"bytes\":{b}"))
                    .unwrap_or_default();
                let quota = used
                    .iter()
                    .find(|(n, t, _)| n == node && t == name)
                    .map(|(_, _, q)| q.as_str())
                    .unwrap_or_default();
                if node.is_empty() {
                    return format!("{{\"name\":{},\"state\":\"active\"{region}}}", quote(name));
                }
                format!(
                    "{{\"name\":{},\"node\":{},\"state\":\"{}\"{region}{replica}{bytes}{quota}}}",
                    quote(name),
                    quote(node),
                    if *moving { "moving" } else { "active" }
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
        let (region, zone) = (place_field(body, "region")?, place_field(body, "zone")?);
        let mut node = Node {
            addr: addr.into(),
            token: token.into(),
            ..Node::default()
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
        // A region or a zone left out is the one recorded: a node given a
        // new token keeps where it is.
        let was = dir.node(name).cloned();
        node.region = region.map_or_else(
            || was.as_ref().map(|w| w.region.clone()).unwrap_or_default(),
            str::to_string,
        );
        node.zone = zone.map_or_else(
            || was.as_ref().map(|w| w.zone.clone()).unwrap_or_default(),
            str::to_string,
        );
        // A region's router keeps its own region's nodes, a node named
        // without one among them; the global router none of a region
        // keeping its own directory ([`region`]).
        match &self.cfg.region {
            Some(own) if region.is_none() && was.is_none() => node.region = own.clone(),
            Some(own) if node.region != *own => {
                return Err(Fail(
                    409,
                    format!(
                        "this router keeps {}'s directory: a node of {} is recorded by that \
                         region's router, or the global one",
                        region_name(own),
                        region_name(&node.region)
                    ),
                ))
            }
            None if dir.routers(&node.region).is_some() => {
                return Err(Fail(
                    409,
                    format!(
                        "{} keeps its own directory: its nodes are recorded by its router",
                        region_name(&node.region)
                    ),
                ))
            }
            _ => {}
        }
        if let Some(w) = was.as_ref().filter(|w| w.region != node.region) {
            if dir.holds(name) {
                return Err(Fail(
                    409,
                    format!(
                        "node `{name}` holds tenants or their copies in {}: its region \
                         changes once they are moved off it and it is in no pair",
                        region_name(&w.region)
                    ),
                ));
            }
        }
        if let Some(s) = &standby {
            // A failover promotes a node's tenants on its standby: in
            // another region, it would carry them out of theirs.
            let theirs = dir.region_of(s);
            if theirs != node.region {
                return Err(Fail(
                    409,
                    format!(
                        "`{s}` is in {}, `{name}` in {}: a standby is kept in its node's \
                         region",
                        region_name(theirs),
                        region_name(&node.region)
                    ),
                ));
            }
        }
        dir.set_node(name, node).map_err(internal)?;
        if let Some(s) = &standby {
            dir.set_standby(name, Some(s)).map_err(internal)?;
            if dir.same_zone(name, s) {
                fenec_http::log!(
                    "node `{name}`'s standby `{s}` is in its zone `{}`: a datacenter lost \
                     takes both",
                    dir.node(name).map_or("", |n| n.zone.as_str())
                );
            }
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
                    .or_else(|| self.on_standby(name, "POST", &format!("{base}/follow")))
                    .or_else(|| self.quota_on_standby(t, name));
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
        // Named, so a node a newer term leases refuses to follow at the word
        // of a router it superseded.
        let holder = self.holder_header();
        let named = [(fenec_http::lease::HOLDER_HEADER, holder.as_str())];
        match self
            .pool
            .call_with(&n.addr, method, target, &n.token, &named, b"")
        {
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
    ///
    /// Only a node of `primary`'s region: a failover promotes the replica,
    /// and one in another region would carry the tenant out of its own. And
    /// a node of another zone before all of that, where the region has one
    /// free: a datacenter lost would take a replica in the primary's zone
    /// with it, which is worth more than a copy already there or an even
    /// spread. A zone not named is ranked between, neither known to be
    /// another nor the same.
    fn replica_node(&self, primary: &str, holding: &[String]) -> Option<String> {
        let dir = self.read_dir();
        let (tenants, _) = dir.load_by_node();
        let replicas = dir.replicas_by_node();
        let from = dir.replicas_from(primary);
        let region = dir.region_of(primary);
        let zone = dir.node(primary).map_or("", |n| n.zone.as_str());
        let held = |n: &str| tenants.get(n).unwrap_or(&0) + replicas.get(n).unwrap_or(&0);
        let free = |n: &&String| {
            n.as_str() != primary
                && dir.region_of(n) == region
                && dir.standby(n).is_none()
                && !dir.is_standby(n)
        };
        let apart = |n: &str| match (zone, dir.node(n).map_or("", |n| n.zone.as_str())) {
            ("", _) | (_, "") => 1,
            (a, b) if a != b => 0,
            _ => 2,
        };
        dir.nodes()
            .keys()
            .filter(free)
            .min_by_key(|n| {
                (
                    apart(n),
                    !holding.contains(n),
                    *from.get(n.as_str()).unwrap_or(&0),
                    held(n),
                    n.to_string(),
                )
            })
            .cloned()
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
            let holder = self.holder_header();
            let named = [(fenec_http::lease::HOLDER_HEADER, holder.as_str())];
            match self
                .pool
                .call_with(&r.addr, method, &target, &r.token, &named, body)
            {
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
        // Its quota, so a failover onto it finds the tenant held as it was.
        if let Some(w) = self.send_quota(tenant, on) {
            return Err(w);
        }
        let mut dir = self.write_dir();
        dir.set_replica(tenant, Some(on)).map_err(|e| message(&e))?;
        // Placed in its primary's zone only where the region had no node
        // free in another (`replica_node`): said here, and counted by
        // `fenec_router_same_zone_copies` for as long as it stays.
        if dir.same_zone(primary, on) {
            fenec_http::log!(
                "tenant `{tenant}`'s replica on `{on}` is in its primary's zone `{}`: {} has \
                 no node free in another, and a datacenter lost takes both",
                dir.node(on).map_or("", |n| n.zone.as_str()),
                region_name(dir.region_of(on))
            );
        }
        Ok(())
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
        Some(self.place_replica(tenant, primary, &[]))
    }

    /// Gives `tenant` a replica on the node [`Self::replica_node`] picks:
    /// the node it went on, or why none did.
    fn place_replica(
        &self,
        tenant: &str,
        primary: &str,
        holding: &[String],
    ) -> std::result::Result<String, String> {
        match self.replica_node(primary, holding) {
            None => Err(match self.read_dir().region_of(primary) {
                "" => "there is no other node to keep it".into(),
                r => format!("there is no other node in {} to keep it", region_name(r)),
            }),
            Some(on) => self.attach(tenant, primary, &on).map(|_| on),
        }
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
            match self.place_replica(&tenant, &primary, &copies) {
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
        // Named, so a node a newer term leases refuses a promotion from a
        // router it superseded.
        let holder = self.holder_header();
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
            let named = [(fenec_http::lease::HOLDER_HEADER, holder.as_str())];
            let answer =
                self.pool
                    .call_with(&standby.addr, "POST", &target, &standby.token, &named, b"");
            if matches!(answer, Ok((200, _))) {
                self.quota_after_failover(&tenant, &to);
            }
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
        // Named, so a node a newer term leases refuses a promotion from a
        // router it superseded.
        let holder = self.holder_header();
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
            let named = [(fenec_http::lease::HOLDER_HEADER, holder.as_str())];
            match self
                .pool
                .call_with(&node.addr, "POST", &target, &node.token, &named, b"")
            {
                Ok((200, _)) => {
                    self.quota_after_failover(&tenant, &to);
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

    /// `PUT /_shard/tenants/<t>/quota`: what is set for the tenant over its
    /// node's quota -- the whole set, replacing the one before; `{}` leaves
    /// it at the node's. Its own node takes it first, and one it refuses
    /// records nothing; then the directory keeps it, and the tenant's
    /// standby and replica are sent it, so a failover finds it there. A
    /// copy that does not answer is sent it again as it is promoted.
    fn set_quota(&self, tenant: &str, body: &[u8]) -> Outcome<Response> {
        let set = fenec_http::quota::Set::parse(&String::from_utf8_lossy(body))
            .map_err(|e| Fail(400, e))?;
        let _claim = self.claim(tenant)?;
        let node = self
            .read_dir()
            .placement(tenant)
            .map(|p| p.node.clone())
            .ok_or_else(|| Fail(404, format!("no tenant `{tenant}` in the directory")))?;
        let n = self.node(&node)?;
        let json = set.json();
        let report = expect(
            self.pool.call(
                &n.addr,
                "PUT",
                &format!("/_admin/tenants/{tenant}/quota"),
                &n.token,
                json.as_bytes(),
            ),
            200,
            &node,
        )?;
        self.write_dir()
            .set_quota(tenant, (!set.is_empty()).then_some(json.as_str()))
            .map_err(internal)?;
        let copies = [
            self.read_dir().standby(&node).map(str::to_string),
            self.read_dir().replica(tenant).map(str::to_string),
        ];
        for copy in copies.into_iter().flatten() {
            if let Some(w) = self.put_quota(tenant, &copy, &json) {
                fenec_http::log!("tenant `{tenant}`'s quota did not reach its copy: {w}");
            }
        }
        Ok(Response::json(200, report))
    }

    /// `GET /_shard/tenants/<t>/quota`: its node's report -- what is set,
    /// what it is held to, what it uses.
    fn quota_report(&self, tenant: &str) -> Outcome<Response> {
        let node = self
            .read_dir()
            .placement(tenant)
            .map(|p| p.node.clone())
            .ok_or_else(|| Fail(404, format!("no tenant `{tenant}` in the directory")))?;
        let n = self.node(&node)?;
        let target = format!("/_admin/tenants/{tenant}/quota");
        let report = expect(
            self.pool.call(&n.addr, "GET", &target, &n.token, b""),
            200,
            &node,
        )?;
        Ok(Response::json(200, report))
    }

    /// Sends `node` the quota the directory keeps for `tenant`, where one
    /// is kept: what went wrong, if anything. A tenant with none sends
    /// nothing, and a create, a move or a failover costs it no call.
    fn send_quota(&self, tenant: &str, node: &str) -> Option<String> {
        let json = self.read_dir().quota(tenant)?.to_string();
        self.put_quota(tenant, node, &json)
    }

    fn put_quota(&self, tenant: &str, node: &str, json: &str) -> Option<String> {
        let Some(n) = self.read_dir().node(node).cloned() else {
            return Some(format!("no node `{node}`"));
        };
        let target = format!("/_admin/tenants/{tenant}/quota");
        match self
            .pool
            .call(&n.addr, "PUT", &target, &n.token, json.as_bytes())
        {
            Ok((200, _)) => None,
            Ok((status, body)) => Some(format!(
                "`{node}` answered {status}: {}",
                String::from_utf8_lossy(&body).trim()
            )),
            Err(e) => Some(format!("`{node}` did not answer: {e}")),
        }
    }

    /// [`Self::send_quota`] to the standby of `node`, where it has one.
    fn quota_on_standby(&self, tenant: &str, node: &str) -> Option<String> {
        let standby = self.read_dir().standby(node).map(str::to_string)?;
        self.send_quota(tenant, &standby)
    }

    /// A promoted copy is sent the tenant's quota again: one it missed --
    /// a copy down when the quota was set -- would otherwise take writes
    /// with none. The failover goes on either way: a tenant taking writes
    /// unbounded is better than one taking none, and it is logged.
    fn quota_after_failover(&self, tenant: &str, to: &str) {
        if let Some(w) = self.send_quota(tenant, to) {
            fenec_http::log!("tenant `{tenant}` failed over to `{to}` without its quota: {w}");
        }
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

    /// Waits until every standby router keeping up has been sent the
    /// directory as it stands, a second at most ([`replication::SENT_WAIT`]):
    /// nothing where none follows. Sent, not applied -- the bytes are on
    /// their way, which the standby applies as they land.
    fn standbys_sent(&self) {
        if let Some(feed) = self.repl.as_ref().and_then(|r| r.feed()) {
            feed.wait_sent(feed.seq(), replication::SENT_WAIT);
        }
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

    /// The node with the fewest bytes on disk among those that answer, in
    /// `region` where one is asked for, `besides` left out (a move's
    /// source). Disk rather than memory: memory counts only the open
    /// tenants, and which ones are open is a matter of who asked last.
    fn least_loaded(&self, region: Option<&str>, besides: Option<&str>) -> Outcome<String> {
        // A standby holds its primary's tenants as replicas: one created
        // there would follow a tenant its primary does not have, and refuse
        // every write. With no disk of its own yet it looked the least
        // loaded, and took every tenant nobody placed.
        let nodes: Vec<(String, Node)> = {
            let dir = self.read_dir();
            let mut there = dir
                .nodes()
                .iter()
                .filter(|(_, n)| region.is_none_or(|r| n.region == r))
                .peekable();
            if let (Some(r), None) = (region, there.peek()) {
                return Err(Fail(
                    409,
                    format!(
                        "no node in {}: add one with PUT /_shard/nodes/<name> \
                         {{\"addr\": .., \"token\": .., \"region\": \"{r}\"}}",
                        region_name(r)
                    ),
                ));
            }
            there
                .filter(|(k, _)| !dir.is_standby(k) && Some(k.as_str()) != besides)
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        };
        if nodes.is_empty() {
            return Err(Fail(
                409,
                match region {
                    None => "no nodes: add one with PUT /_shard/nodes/<name>".into(),
                    Some(r) => format!(
                        "no other node in {} takes tenants: a standby takes none",
                        region_name(r)
                    ),
                },
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

    /// Places and creates a tenant: on the node `at` names, or on the one
    /// with the fewest bytes in the region it names -- `--default-region`
    /// where it names neither, and any region without one.
    fn create(
        &self,
        tenant: &str,
        at: At<'_>,
        schema: Option<&str>,
        quota: Option<fenec_http::quota::Set>,
    ) -> Outcome<Response> {
        fenec_http::tenants::check_name(tenant).map_err(|r| Fail(r.0, r.1))?;
        // Read before anything is placed: a schema that does not read
        // leaves no tenant behind.
        let schema = schema
            .map(fenec_http::admin::schema_description)
            .transpose()
            .map_err(|e| Fail(400, e))?;
        let _claim = self.claim(tenant)?;
        // A name at home in a region keeping its own directory is taken too.
        if self.read_dir().exists(tenant) {
            return Err(Fail(409, format!("tenant `{tenant}` already exists")));
        }
        let region = match at.node {
            None => at.region.or(self.cfg.default_region.as_deref()),
            Some(_) => at.region,
        };
        let name = match at.node {
            Some(n) => n.to_string(),
            None => self.least_loaded(region, None)?,
        };
        let n = self.node(&name)?;
        if let Some(r) = region.filter(|r| *r != n.region) {
            return Err(Fail(
                409,
                format!(
                    "node `{name}` is in {}, not {}",
                    region_name(&n.region),
                    region_name(r)
                ),
            ));
        }
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
        // The quota, before anything is written: the tenant takes its first
        // write held to it. Recorded here first, which is what every node
        // the tenant is placed on later is sent; one the node refuses takes
        // the tenant back off it, as a schema refused does.
        if let Some(set) = quota.filter(|q| !q.is_empty()) {
            let json = set.json();
            let sent = self
                .write_dir()
                .set_quota(tenant, Some(&json))
                .map_err(internal);
            let sent = sent.and_then(|_| {
                expect(
                    self.pool.call(
                        &n.addr,
                        "PUT",
                        &format!("/_admin/tenants/{tenant}/quota"),
                        &n.token,
                        json.as_bytes(),
                    ),
                    200,
                    &name,
                )
            });
            if let Err(e) = sent {
                let _ = self.pool.call(&n.addr, "DELETE", &target, &n.token, b"");
                if let Err(e) = self.write_dir().remove_tenant(tenant) {
                    fenec_http::log!("tenant `{tenant}` stayed in the directory: {e}");
                }
                self.lease_now(&name);
                return Err(e);
            }
        }
        // The schema, applied on the node with its admin token once the
        // lease names the tenant, which a write needs: the app that creates
        // tenants holds the router's token alone, where it held the nodes'
        // data token, which reaches every tenant, to apply it itself. A
        // schema refused takes the tenant back off the node and out of the
        // directory: a tenant is there with its schema or not at all.
        let applied = match &schema {
            None => None,
            Some(d) => {
                let target = format!("/_admin/tenants/{tenant}/schema");
                let got = self
                    .pool
                    .call(&n.addr, "POST", &target, &n.token, d.as_bytes());
                match got {
                    Ok((200, body)) => Some(String::from_utf8_lossy(&body).into_owned()),
                    other => {
                        let _ = self.pool.call(
                            &n.addr,
                            "DELETE",
                            &format!("/_admin/tenants/{tenant}"),
                            &n.token,
                            b"",
                        );
                        if let Err(e) = self.write_dir().remove_tenant(tenant) {
                            fenec_http::log!("tenant `{tenant}` stayed in the directory: {e}");
                        }
                        self.lease_now(&name);
                        return Err(match other {
                            Ok((status, body)) => {
                                Fail(status, String::from_utf8_lossy(&body).into_owned())
                            }
                            Err(e) => Fail(502, format!("node `{name}` did not answer: {e}")),
                        });
                    }
                }
            }
        };
        // The replica follows into a file of its own, so the tenant is
        // created on the standby as well.
        let warning = self
            .on_standby(&name, "PUT", &target)
            .or_else(|| self.quota_on_standby(tenant, &name));
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
                "{{\"tenant\":{},\"node\":{}{}{}{}{}}}",
                quote(tenant),
                quote(&name),
                region_field(&n.region),
                match &applied {
                    None => String::new(),
                    Some(o) => format!(",\"schema\":{o}"),
                },
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
    ///
    /// The target is the node `at` names, or the one with the fewest bytes
    /// in the region it names. A tenant leaves its region only when the
    /// request names the region it goes to: its residency is a promise, and
    /// a node's name alone does not say it breaks one.
    fn relocate(&self, tenant: &str, at: At<'_>) -> Outcome<Response> {
        let _claim = self.claim(tenant)?;
        let started = Instant::now();
        let from = self
            .read_dir()
            .placement(tenant)
            .map(|p| p.node.clone())
            .ok_or_else(|| Fail(404, format!("no tenant `{tenant}` in the directory")))?;
        let to = match at.node {
            Some(n) => n.to_string(),
            None => self.least_loaded(at.region, Some(&from))?,
        };
        let to = to.as_str();
        {
            let dir = self.read_dir();
            let (here, there) = (dir.region_of(&from), dir.region_of(to));
            match at.region {
                _ if dir.node(to).is_none() => {}
                Some(r) if r != there => {
                    return Err(Fail(
                        409,
                        format!(
                            "`{to}` is in {}, not {}",
                            region_name(there),
                            region_name(r)
                        ),
                    ))
                }
                None if here != there => {
                    return Err(Fail(
                        409,
                        format!(
                            "`{to}` is in {}, `{tenant}` in {}: a move across regions names \
                             the region it goes to, {{\"to\": \"{to}\", \"region\": \
                             \"{there}\"}}",
                            region_name(there),
                            region_name(here)
                        ),
                    ))
                }
                _ => {}
            }
        }
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
        // Its quota goes with it, before the directory points there: the
        // first write the target takes is held to it.
        if let Some(w) = self.send_quota(tenant, to) {
            let _ = self.pool.call(&dst.addr, "DELETE", &base, &dst.token, b"");
            return Err(thaw(Fail(502, w)));
        }

        if let Err(e) = self.write_dir().place(tenant, to, State::Active) {
            let _ = self.pool.call(&dst.addr, "DELETE", &base, &dst.token, b"");
            return Err(thaw(internal(e)));
        }
        self.lease_now(to);

        // The replica follows the node the tenant is on now: a file there,
        // and the copy on the old node's standby is not this tenant's any
        // more.
        if let Some(w) = self
            .on_standby(to, "PUT", &base)
            .or_else(|| self.quota_on_standby(tenant, to))
        {
            fenec_http::log!("tenant `{tenant}` has no replica on `{to}` yet: {w}");
        }
        if let Some(w) = self.on_standby(&from, "DELETE", &base) {
            fenec_http::log!("tenant `{tenant}`'s replica on `{from}`'s standby stayed: {w}");
        }

        // Its replica follows it there: the image carried the history, so
        // the copy goes on from where it stood. One that will not is taken
        // off, and the tenant given another -- and so is one that no longer
        // fits: in the region the tenant left, or in its new node's zone
        // where the region has a node free in another. Kept there, a
        // failover would promote the tenant back out of its region, or a
        // datacenter lost would take both copies.
        let fits = |r: &String| self.replica_node(to, std::slice::from_ref(r)).as_ref() == Some(r);
        let own = match replica {
            Some(r) if !fits(&r) => {
                fenec_http::log!(
                    "tenant `{tenant}`'s replica on `{r}` does not fit its node `{to}`: \
                     placed again"
                );
                if let Some(w) = self.detach(tenant) {
                    fenec_http::log!("tenant `{tenant}`'s copy on `{r}` stayed: {w}");
                }
                self.replicate(tenant, to)
            }
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
        // as it goes, and the clients reconnect through the router -- a
        // standby router too, which routes from the flip once it has it: the
        // copy goes once every standby keeping up has been sent it, or one
        // in another region forwarded to a node that held the tenant no
        // more, and its client was told 404.
        self.standbys_sent();
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
                "{{\"tenant\":{},\"from\":{},\"to\":{}{},\"bytes\":{},\"ms\":{},\"source_removed\":{}{}}}",
                quote(tenant),
                quote(&from),
                quote(to),
                region_field(&dst.region),
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

/// The route a request is counted under, by its first segment.
fn route_of(req: &Request) -> Route {
    match req.segments().first() {
        Some(&"t") => Route::Tenant,
        Some(&"_shard") => Route::Shard,
        Some(&"_replication") => Route::Replication,
        Some(&"_metrics") => Route::Metrics,
        _ => Route::Other,
    }
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

/// The `quota` member of a create's body, an object, where it has one.
fn quota_member(
    body: &[(String, Value)],
) -> std::result::Result<Option<fenec_http::quota::Set>, String> {
    match body.iter().find(|(k, _)| k == "quota").map(|(_, v)| v) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(members)) => {
            fenec_http::quota::Set::from_members(members.clone()).map(Some)
        }
        Some(_) => Err("`quota` is an object: {\"bytes\": .., \"rows\": .., ...}".into()),
    }
}

/// Where a create or a move asks a tenant to go: a node, a region, or both.
#[derive(Clone, Copy, Default)]
struct At<'a> {
    node: Option<&'a str>,
    region: Option<&'a str>,
}

/// A region's or a zone's name in a body, where it has one: a tenant's
/// name's rule, so it goes into a message, a label and a log as it is.
/// `""` is the unnamed region (or zone), which the nodes of a directory
/// that names none are in.
fn place_field<'a>(body: &'a [(String, Value)], key: &str) -> Outcome<Option<&'a str>> {
    match body.iter().find(|(k, _)| k == key).map(|(_, v)| v) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Text(s)) if s.is_empty() => Ok(Some("")),
        Some(Value::Text(s)) => match fenec_http::tenants::check_name(s) {
            Ok(()) => Ok(Some(s)),
            Err(_) => Err(Fail(
                400,
                format!(
                    "invalid {key} `{s}`: [a-z0-9_-], 1 to {} characters",
                    fenec_http::tenants::MAX_NAME
                ),
            )),
        },
        Some(_) => Err(Fail(400, format!("`{key}` is a name, a text"))),
    }
}

/// A region as a message names it.
fn region_name(region: &str) -> String {
    match region {
        "" => "the unnamed region".into(),
        r => format!("region `{r}`"),
    }
}

/// A tenant's region as an answer carries it: left out where unnamed, as
/// it was before regions.
fn region_field(region: &str) -> String {
    match region {
        "" => String::new(),
        r => format!(",\"region\":{}", quote(r)),
    }
}

/// A per-node sample, the node's region beside its name where it names
/// one: an empty label is a label absent to Prometheus, so a directory
/// that names no region scrapes as it did.
fn node_sample(
    out: &mut fenec_http::metrics::Text,
    name: &str,
    dir: &Directory,
    node: &str,
    value: impl std::fmt::Display,
) {
    match dir.region_of(node) {
        "" => out.sample(name, &[("node", node)], value),
        r => out.sample(name, &[("node", node), ("region", r)], value),
    }
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
        | Error::Duplicate(m)
        | Error::Type(m)
        | Error::Query(m)
        | Error::Corrupt(m)
        | Error::Io(m)
        | Error::Plugin(m)
        | Error::ReadOnly(m)
        | Error::Denied(m)
        | Error::Unmet(m)
        | Error::Quota(m) => m.clone(),
    }
}

fn internal(e: Error) -> Fail {
    Fail(500, format!("directory: {}", message(&e)))
}

fn read(d: &RwLock<Directory>) -> std::sync::RwLockReadGuard<'_, Directory> {
    d.read().unwrap_or_else(|e| e.into_inner())
}

fn write(d: &RwLock<Directory>) -> std::sync::RwLockWriteGuard<'_, Directory> {
    d.write().unwrap_or_else(|e| e.into_inner())
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
