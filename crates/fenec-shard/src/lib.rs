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
pub mod readers;
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
    /// The region this router stands in, for the reads its clients mark
    /// (`--near`): such a read of a tenant with a read replica here goes
    /// to it ([`readers`]). A region's router (`region`) is near its
    /// region unless told otherwise; `None` with neither, every read goes
    /// home.
    pub near: Option<String>,
    /// How long a read replica has to answer a marked read, past the wait
    /// its `Fenec-After` asks for (`--replica-timeout`): the connect, every
    /// write of the request and every read of its answer's head. One that
    /// does not is let be for [`readers::RESTING`] -- its node, not
    /// reached, or not answering its `/_health` either within
    /// [`readers::HEALTH_WAIT`] (this where shorter); its tenant's copy
    /// alone where the node answers, the read slower than this -- and the
    /// read goes home: at `upstream_timeout` a hung replica's node held
    /// each marked read a minute. A node that answers its health while
    /// every query on it stalls, its disk stalled under them, costs each
    /// tenant with a copy there one timeout a rest.
    pub replica_timeout: Duration,
    /// How long a read replica that did not answer a marked read is let be
    /// ([`readers::RESTING`] unless set).
    pub replica_rest: Duration,
    /// The longest a call a read replica's upkeep makes may take
    /// ([`readers::CALL`] unless set).
    pub reader_timeout: Duration,
    /// The global entry's token (`--entry-token`, its `ENTRY_TOKEN`): a
    /// request whose `Fenec-Client` header carries the entry's mark for
    /// its address and a time within a minute of this clock is the named
    /// client's, not the connection's -- its refusal's wait and count, a
    /// verified token's clearing, the audit line, the trace and the client
    /// named to a node or a relay ([`fenec_http::audit::entry`]). `None`:
    /// the header is read by no one, and dropped as every router drops it.
    pub entry_token: Option<String>,
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
            near: None,
            replica_timeout: Duration::from_secs(2),
            replica_rest: readers::RESTING,
            reader_timeout: readers::CALL,
            entry_token: None,
        }
    }
}

pub struct Router {
    /// The router itself, for the work it hands to threads of their own
    /// from wherever it is asked for: a lease grant, a failover, a repair.
    me: std::sync::Weak<Router>,
    /// A repair wanted, and whether its thread runs ([`Router::repair_soon`]).
    repairs: Mutex<(bool, bool)>,
    /// The repairs run again for tenants a repair passed over because
    /// their primary did not answer, and whether one is waited for
    /// ([`Router::repair_later`]).
    again: Mutex<(u32, bool)>,
    dir: RwLock<Directory>,
    /// The directory's own replication: the feed a standby reads, and the
    /// follower a standby runs. `None` without --replication-token.
    repl: Option<Arc<Replication>>,
    pool: Pool,
    cfg: Config,
    /// Tenants with a create, move or delete in progress: a second
    /// operation on the same tenant is refused rather than interleaved.
    busy: Mutex<HashSet<String>>,
    /// Told as a tenant's operation lets it go: what a failover on a lease
    /// waits on for a tenant it had to leave ([`Router::fail_over_held`]).
    freed: std::sync::Condvar,
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
    /// The key the global entry's marks are checked with
    /// ([`Config::entry_token`]), its padded blocks hashed once.
    entry: Option<fenec_http::audit::EntryKey>,
    /// For each region, the router of its list that answered last: asked
    /// first, so a router gone costs its region one failed connect, not one
    /// a request.
    answered: Mutex<std::collections::HashMap<String, usize>>,
    /// The read replicas this router last failed to point at their
    /// tenant's primary (`<tenant>/<node>`): said once in the log, not at
    /// every round of [`readers`], and again once one succeeds.
    unfollowed: Mutex<HashSet<String>>,
    /// The read replicas whose change is under way (`<tenant>/<region>`):
    /// placed, taken off, pointed at a new primary. One change of each at
    /// a time, so that a look over them never puts back one taken off
    /// meanwhile; held across the calls to its node, so no tenant's claim
    /// and no lock a move or a failover takes -- a node a region away, or
    /// cut off, held those up for as long as it did not answer.
    reader_busy: Mutex<HashSet<String>>,
    /// The read replicas a marked read, or the check of their nodes
    /// ([`readers::CHECK_EVERY`]), found gone from their node
    /// (`<tenant>/<node>`): made there again at the next look.
    reader_lost: Mutex<HashSet<String>>,
    /// The quota each read replica (`<tenant>/<node>`) was last sent, `None`
    /// for the node's own: a look wants it sent again only once its
    /// tenant's changes ([`readers`]).
    reader_quotas: Mutex<std::collections::HashMap<String, Option<String>>>,
    /// The read replicas' nodes a look's worker is under way on
    /// ([`Router::follow_round`]): one a node at a time.
    lookers: Mutex<HashSet<String>>,
    /// The tenants this directory places whose lineage is to be had from
    /// their node ([`Router::backfill_lineages`]).
    lineage_wanted: Mutex<HashSet<String>>,
    /// The nodes' answers [`Router::backfill_lineages`] has taken.
    lineage_answers: std::sync::atomic::AtomicU64,
    /// How many times in a row each read replica's lost copy was made
    /// again ([`Router::may_remake`]), by `<tenant>/<node>`.
    reader_remade: Mutex<std::collections::HashMap<String, u32>>,
    /// The quotas the read replicas are to be sent, and the nodes a sender
    /// is under way to ([`readers::QuotaQueue`]).
    reader_quota_queue: Mutex<readers::QuotaQueue>,
    /// Set, and its condvar told, when a quota is wanted: the senders start
    /// now rather than at the next [`readers::CHECK_EVERY`].
    quotas_soon: (Mutex<bool>, std::sync::Condvar),
    /// The read replicas' nodes that were not reached, and tenants' copies
    /// that did not answer a marked read in time (`<tenant>/<node>`), and
    /// until when their reads go home ([`readers::RESTING`]): one that
    /// hangs would have every read wait its [`Config::replica_timeout`]
    /// first. One that answers, a 503 that it has not caught up among
    /// them, costs a read a hop alone.
    resting: Arc<Mutex<std::collections::HashMap<String, Instant>>>,
    /// Tenants' copies that answered a marked read 504, behind the position
    /// it was sent after at the end of its wait (`<tenant>/<node>`): until
    /// when, and the longest wait they could not meet
    /// ([`Router::rest_behind`]).
    behind: Mutex<std::collections::HashMap<String, (Instant, Duration)>>,
    /// The read replicas' nodes being asked their `/_health` now, one
    /// question a node at a time ([`Router::rest_unanswered`]): shared with
    /// the thread asking it, as `resting` is.
    probing: Arc<Mutex<HashSet<String>>>,
    /// What the read replicas' calls to their nodes go through: bounded by
    /// [`readers::CALL`], not `upstream_timeout`, so a node that does not
    /// answer holds a look over them up that long at most.
    reader_pool: Pool,
    /// Set, and its condvar told, when a failover or a move changed a
    /// primary: the next look over the read replicas comes now rather than
    /// at its time, and the failover waits on no replica's node.
    follow_soon: (Mutex<bool>, std::sync::Condvar),
}

/// Ends a round's grant to a node however the grant ends, a panic in it
/// too: left marked as under way, the node would never be granted by a
/// round again ([`lease::Leases::begin_grant`]).
struct Granting<'a>(&'a Router, &'a str);

impl Drop for Granting<'_> {
    fn drop(&mut self) {
        if let Some((leases, _)) = &self.0.leases {
            leases.end_grant(self.1);
        }
    }
}

/// Takes the repair thread's mark off where a repair panicked -- left on,
/// no repair would run again ([`Router::repair_soon`]) -- and starts another
/// thread where a repair was wanted meanwhile, which the dying thread would
/// have run. A thread that ends as it should takes the mark off itself,
/// under the lock that found nothing wanted.
struct Repairing<'a>(&'a Router);

impl Drop for Repairing<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let again = {
                let mut wanted = self.0.repairs.lock().unwrap_or_else(|e| e.into_inner());
                wanted.1 = false;
                wanted.0
            };
            if again {
                self.0.repair_soon();
            }
        }
    }
}

/// How long the first repair run again for a tenant whose primary did not
/// answer waits ([`Router::repair_later`]), doubled for each more up to
/// [`REPAIR_AGAIN_MOST`], [`REPAIR_AGAIN_TRIES`] of them: about four minutes
/// in all. A repair is otherwise run only when a node failed over answers
/// again or an operator asks (`POST /_shard/replicas`), so a primary that
/// missed the one look left its tenant with no replica for good.
const REPAIR_AGAIN_FIRST: Duration = Duration::from_secs(2);
const REPAIR_AGAIN_MOST: Duration = Duration::from_secs(60);
const REPAIR_AGAIN_TRIES: u32 = 8;

/// How long a failover on a lease waits for the tenants another operation
/// held through it ([`Router::fail_over_held`]): an operation is bounded by
/// its calls' timeouts, a move of a large tenant among them, and one that
/// never lets go -- a bug -- would keep a thread waking for good.
const HELD_WAIT: Duration = Duration::from_secs(3600);

/// A tenant a failover left because another operation held it, and the
/// copy it would have been promoted on as the failover ran -- its node's
/// standby, or its own replica (`own_replica`); `None` where it had none.
/// Pinned then, and promoted on that copy alone ([`Router::fail_over_held`]):
/// read again once the operation let the tenant go, the replica was the one
/// a repair holding the tenant had just attached, following the node gone,
/// and the tenant was promoted onto a copy that never took a byte -- its
/// rows wiped from its old node once that came back and followed it.
struct HeldTenant {
    tenant: String,
    to: Option<String>,
    own_replica: bool,
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
        // An empty token is none: `/_shard/` read a request with no
        // `Authorization` as the empty token, and the relay's mark made of
        // it was anyone's to make. As none, the router asks for none on
        // loopback and refuses an address past it.
        let mut cfg = cfg;
        cfg.token = cfg.token.filter(|t| !t.is_empty());
        cfg.entry_token = cfg.entry_token.filter(|t| !t.is_empty());
        Arc::new_cyclic(|me| Router {
            me: me.clone(),
            repairs: Mutex::new((false, false)),
            again: Mutex::new((0, false)),
            dir: RwLock::new(dir),
            repl,
            pool: Pool::new(cfg.upstream_timeout),
            busy: Mutex::new(HashSet::new()),
            freed: std::sync::Condvar::new(),
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
            entry: cfg
                .entry_token
                .as_deref()
                .map(fenec_http::audit::EntryKey::new),
            answered: Mutex::new(Default::default()),
            unfollowed: Mutex::new(HashSet::new()),
            reader_busy: Mutex::new(HashSet::new()),
            reader_lost: Mutex::new(HashSet::new()),
            reader_quotas: Mutex::new(Default::default()),
            lookers: Mutex::new(HashSet::new()),
            lineage_wanted: Mutex::new(HashSet::new()),
            lineage_answers: std::sync::atomic::AtomicU64::new(0),
            reader_remade: Mutex::new(std::collections::HashMap::new()),
            reader_quota_queue: Mutex::new(Default::default()),
            quotas_soon: (Mutex::new(false), std::sync::Condvar::new()),
            resting: Arc::new(Mutex::new(Default::default())),
            behind: Mutex::new(Default::default()),
            probing: Arc::new(Mutex::new(HashSet::new())),
            reader_pool: Pool::new(cfg.reader_timeout),
            follow_soon: (Mutex::new(false), std::sync::Condvar::new()),
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
        // The read replicas this router keeps follow their tenants'
        // primaries wherever those go ([`readers`]).
        self.start_following()?;
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
                router.round(false);
            })?;
        Ok(())
    }

    /// One round, waited for: every node's lease renewed, each on a thread
    /// of its own, then a failover of each node whose lease has certainly
    /// lapsed, each on a thread of its own too -- this returning once all
    /// of them have ended. What a test runs; the leasing thread's rounds
    /// wait for none of it (`round`).
    pub fn lease_round(&self) {
        self.round(true);
    }

    /// One round: every node sent its grant on a thread of its own, and
    /// every node whose lease has certainly lapsed failed over on a thread
    /// of its own. `wait`: this returns once all of them have ended.
    ///
    /// The leasing thread's rounds wait for nothing. They waited for every
    /// grant before they looked for a lapse, so a grant that took long held
    /// up the next round's renewals and the lapse of every other node: a
    /// node recorded by a name its resolver was slow to refuse once it was
    /// gone -- the lookup bounded by nothing -- had the region's healthy
    /// nodes go unrenewed until their leases lapsed (V5's drill: 18 s
    /// without a write in CI), and the failovers and repairs that ran on
    /// this thread after it waited on the same lookup. Now one node's grant
    /// holds up nothing but its own next grant (a round sends a node none
    /// while one of its own is under way, [`lease::Leases::begin_grant`])
    /// and its own failover, which waits for it in the node's turn.
    fn round(&self, wait: bool) {
        let Some((leases, _)) = &self.leases else {
            return;
        };
        // A standby router's directory follows the primary's, which leases.
        if self.read_dir().following() {
            leases.standby();
            return;
        }
        leases.leading();
        let Some(me) = self.me.upgrade() else {
            return;
        };
        let (lists, reading, holder) = self.primaries_read(leases);
        let holder = Arc::new(holder);
        let mut grants = Vec::new();
        for (node, list) in lists {
            leases.know(&node);
            if !wait && !leases.begin_grant(&node) {
                continue;
            }
            let (router, holder, name) = (Arc::clone(&me), Arc::clone(&holder), node.clone());
            let spawned = std::thread::Builder::new()
                .name("fenec-lease-grant".into())
                .spawn(move || {
                    // Ended however the grant ends, a panic in it too: left
                    // set, the node would never be granted again.
                    let _ended = (!wait).then(|| Granting(&router, &node));
                    router.grant(&node, &list, reading, &holder);
                });
            match spawned {
                Ok(h) => grants.push(h),
                Err(e) => {
                    if !wait {
                        leases.end_grant(&name);
                    }
                    fenec_http::log!("no thread for a lease grant to `{name}`: {e}");
                }
            }
        }
        if wait {
            for g in grants {
                let _ = g.join();
            }
        }
        let mut failovers = Vec::new();
        for node in leases.suspects(Instant::now()) {
            let router = Arc::clone(&me);
            let name = node.clone();
            let spawned = std::thread::Builder::new()
                .name("fenec-failover".into())
                .spawn(move || router.fail_over_lapsed(&node));
            match spawned {
                Ok(h) => failovers.push(h),
                Err(e) => {
                    leases.unsuspect(&name);
                    fenec_http::log!("no thread for the failover of `{name}`: {e}");
                }
            }
        }
        if wait {
            for f in failovers {
                let _ = f.join();
            }
        }
    }

    /// Fails `node` over if, judged in its turn, its lease has certainly
    /// lapsed. The turn is held through: a grant to it that the router still
    /// waits on ends first, so the lapse is judged on every answer that came
    /// back -- a grant the node took runs its lease on from the moment it
    /// took it -- and no grant over a list read before the failover reaches
    /// it after: the turn is let go of once it was sent the list the
    /// failover left, so an older list queued behind is dropped
    /// ([`lease::Leases::turn`]). The leasing thread ran the failover itself
    /// once its round's grants had ended, so none of theirs was under way;
    /// on a thread of its own, beside the rounds, the turn is what keeps a
    /// grant and the failover apart. A grant the router gave up on is not
    /// counted, though the node may still take it (the lease module's
    /// comment).
    fn fail_over_lapsed(&self, node: &str) {
        let Some((leases, _)) = &self.leases else {
            return;
        };
        let turn = leases.turn(node);
        let sent = leases.wait_turn(&turn);
        if !leases.confirm_lapse(node, Instant::now()) {
            return;
        }
        // A router that follows another now leases nothing: the one it
        // follows judges the node, and a promotion here would be a second.
        if self.read_dir().following() {
            return;
        }
        fenec_http::log!(
            "node `{node}` renewed no lease in {:?}: failing its tenants over",
            lease::lapse(leases.term)
        );
        let mut held = Vec::new();
        let (status, why) = match self.failover(node, &mut held, false) {
            Ok(r) => (r.status, String::from_utf8_lossy(&r.body).into_owned()),
            Err(Fail(status, why)) => (status, why),
        };
        metrics::failed_over(status == 200);
        fenec_http::log!("failover of `{node}`: {status} {why}");
        // The tenants it left with a copy to go to, named by no grant to the
        // node from the one below on, the turn still held.
        held.retain(|h| match &h.to {
            Some(_) => {
                leases.withhold(node, &h.tenant);
                true
            }
            None => {
                fenec_http::log!(
                    "failover of `{node}`: `{}` had no copy to go to as its node was failed \
                     over, and stays there",
                    h.tenant
                );
                false
            }
        });
        if self.granted_after(node, sent) {
            self.answers_again(node);
        } else if !held.is_empty() {
            self.fail_over_held(node, held);
        }
    }

    /// After a failover of `node`, its turn held (`sent`): the node sent the
    /// list the failover left -- first, in its turn -- and the nodes its
    /// tenants went to granted now rather than at the next round, so they
    /// take their writes. Its turn is let go of before the others' grants
    /// are waited for: two nodes failed over at once, each holding its own
    /// turn while waiting for the other's, would wait for good. Whether the
    /// node answered.
    fn granted_after(&self, node: &str, mut sent: std::sync::MutexGuard<'_, u64>) -> bool {
        let Some((leases, _)) = &self.leases else {
            return false;
        };
        let (lists, reading, holder) = self.primaries_read(leases);
        let none = Vec::new();
        let own = lists.get(node).unwrap_or(&none);
        std::thread::scope(|s| {
            for (other, list) in &lists {
                if other != node {
                    let holder = &holder;
                    s.spawn(move || self.grant(other, list, reading, holder));
                }
            }
            let back = self.grant_held(&mut sent, node, own, reading, &holder);
            drop(sent);
            back
        })
    }

    /// The tenants a failover on a lease had to leave because another
    /// operation held them -- a repair giving one a replica, a quota being
    /// set, a move -- each failed over once it is let go of. A failover by
    /// hand says which and is asked again; on a lease nobody asks again,
    /// and such a tenant stayed on a node gone for good. More so once the
    /// repair ran beside the failovers rather than after them on the
    /// leasing thread.
    ///
    /// Each goes to the copy pinned as the failover ran and to no other
    /// ([`HeldTenant`]), and only while that copy is still the one: a copy
    /// attached meanwhile follows the node gone and holds nothing of it.
    /// Withheld from the node's grants since the failover
    /// ([`lease::Leases::withhold`]), each is promoted a lease and a tenth
    /// past the last grant that could name it at the soonest, in the node's
    /// turn as the failover was, and only while the node has answered no
    /// grant since -- one that answers holds its tenants again, and their
    /// copies follow by the repair its answer starts -- and while this
    /// router leases. Given up after [`HELD_WAIT`], or once the node is
    /// gone from the directory: what is left stays where it is, named by
    /// the node's grants again.
    fn fail_over_held(&self, node: &str, mut left: Vec<HeldTenant>) {
        let Some((leases, _)) = &self.leases else {
            return;
        };
        let holder = self.holder_header();
        let until = Instant::now() + HELD_WAIT;
        let give_up = |left: &[HeldTenant], why: &str| {
            for h in left {
                leases.release(node, &h.tenant);
                fenec_http::log!(
                    "failover of `{node}`: `{}` stays where it is ({why})",
                    h.tenant
                );
            }
        };
        loop {
            {
                // Waited on while no tenant left may go yet: each held by
                // another operation, or within its lease past the last grant
                // that named it. Looked at again now and then besides: the
                // node may answer, or a tenant go elsewhere, meanwhile.
                let now = Instant::now();
                let busy = self.busy.lock().unwrap_or_else(|e| e.into_inner());
                let ready = left.iter().any(|h| {
                    !busy.contains(&h.tenant)
                        && leases
                            .withheld_until(node, &h.tenant)
                            .is_none_or(|from| from <= now)
                });
                if !ready {
                    let _ = self.freed.wait_timeout(busy, Duration::from_millis(100));
                }
            }
            if !leases.is_lost(node) {
                return;
            }
            if self.read_dir().node(node).is_none() {
                return give_up(&left, "its node is gone from the directory");
            }
            if self.read_dir().following() {
                return give_up(&left, "this router follows another now");
            }
            if Instant::now() > until {
                return give_up(&left, "the operation holding it outlasted the wait");
            }
            let turn = leases.turn(node);
            let sent = leases.wait_turn(&turn);
            if !leases.is_lost(node) {
                return;
            }
            let before = left.len();
            let mut moved = false;
            left.retain(|h| {
                let tenant = h.tenant.as_str();
                // Moved or deleted meanwhile: nothing of it is left here.
                if self
                    .read_dir()
                    .placement(tenant)
                    .is_none_or(|p| p.node != node)
                {
                    leases.release(node, tenant);
                    return false;
                }
                if leases
                    .withheld_until(node, tenant)
                    .is_some_and(|from| from > Instant::now())
                {
                    return true;
                }
                let Ok(_claim) = self.claim(tenant) else {
                    return true;
                };
                let Some(to) = &h.to else {
                    return false;
                };
                // The copy pinned is still the one the failover would use.
                let current = {
                    let dir = self.read_dir();
                    match h.own_replica {
                        true => dir.replica(tenant) == Some(to.as_str()),
                        false => dir.standby(node) == Some(to.as_str()),
                    }
                };
                if !current {
                    leases.release(node, tenant);
                    fenec_http::log!(
                        "failover of `{node}`: `{tenant}` stays where it is: its copy on `{to}` \
                         is not the one it had as its node was failed over"
                    );
                    return false;
                }
                match self.promote_on(tenant, to, &holder, h.own_replica, false) {
                    Ok(()) => {
                        moved = true;
                        fenec_http::log!(
                            "failover of `{node}`: `{tenant}`, let go of by the operation that \
                             held it, promoted on `{to}`"
                        );
                    }
                    Err(e) => fenec_http::log!("failover of `{node}`: `{tenant}` stays: {e}"),
                }
                leases.release(node, tenant);
                false
            });
            if moved {
                self.follow_soon();
            }
            if left.len() < before {
                // The pair is over once no tenant is left on the node, as at
                // the end of a failover that left none -- promoted, moved or
                // deleted alike.
                let empty = {
                    let dir = self.read_dir();
                    dir.standby(node).is_some() && dir.tenants().iter().all(|(_, p)| p.node != node)
                };
                if empty {
                    if let Err(e) = self.write_dir().set_standby(node, None) {
                        fenec_http::log!("failover of `{node}`: the pair stayed recorded: {e}");
                    }
                }
                if self.granted_after(node, sent) {
                    self.answers_again(node);
                    return;
                }
            } else {
                drop(sent);
            }
            if left.is_empty() {
                return;
            }
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
        let mut lists = dir.primaries();
        leases.withhold_from(&mut lists);
        (lists, leases.reading(), self.holder_of(&dir))
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
    /// `reading`, in its turn: one grant to a node at a time, and none over
    /// a reading older than the last one sent, which the node then holds
    /// the list of ([`lease::Leases::turn`]). A node that answers after it
    /// was failed over has a repair run on a thread of its own
    /// ([`Self::repair_soon`]), which has its copies follow its tenants' new
    /// primaries.
    fn grant(
        &self,
        node: &str,
        primaries: &[String],
        reading: u64,
        holder: &fenec_http::lease::Holder,
    ) {
        let Some((leases, _)) = &self.leases else {
            return;
        };
        let turn = leases.turn(node);
        let mut sent = leases.wait_turn(&turn);
        let back = self.grant_held(&mut sent, node, primaries, reading, holder);
        drop(sent);
        if back {
            self.answers_again(node);
        }
    }

    /// [`Self::grant`] with the node's turn held, `sent` the reading of the
    /// newest list it was sent: whether the node answered after it was
    /// failed over. The list itself goes only when the node may not hold
    /// it, and again with it when the node says it does not.
    fn grant_held(
        &self,
        sent: &mut u64,
        node: &str,
        primaries: &[String],
        reading: u64,
        holder: &fenec_http::lease::Holder,
    ) -> bool {
        let Some((leases, pool)) = &self.leases else {
            return false;
        };
        let Some(n) = self.read_dir().node(node).cloned() else {
            return false;
        };
        if reading < *sent {
            return false;
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
        back
    }

    /// `node` answered a grant after it was failed over: a repair has its
    /// copies follow its tenants' new primaries.
    fn answers_again(&self, node: &str) {
        fenec_http::log!(
            "node `{node}` answers again: its copies are to follow the tenants' new primaries"
        );
        // Granted again at once: the grant it answered left out the tenants
        // a failover withheld from it, which it holds again now.
        self.lease_now(node);
        self.repair_soon();
    }

    /// Has a repair ([`Self::repair`]) run on a thread of its own, and again
    /// once it ends where another was wanted meanwhile: one at a time, none
    /// waited for. A repair asks every node what it holds and attaches a
    /// replica a tenant, each a call that waits as long as its node takes,
    /// and ran on the leasing thread: a node back from a failover had the
    /// repair call a node gone -- its name's lookup alone 5 s a call in V5's
    /// drill -- for 20 s, no lease renewed meanwhile, and the healthy node it
    /// ran for was failed over at its end.
    fn repair_soon(&self) {
        let mut wanted = self.repairs.lock().unwrap_or_else(|e| e.into_inner());
        wanted.0 = true;
        if wanted.1 {
            return;
        }
        let Some(router) = self.me.upgrade() else {
            return;
        };
        wanted.1 = true;
        drop(wanted);
        let spawned = std::thread::Builder::new()
            .name("fenec-repair".into())
            .spawn(move || {
                let _panicked = Repairing(&router);
                loop {
                    {
                        let mut wanted = router.repairs.lock().unwrap_or_else(|e| e.into_inner());
                        // Its mark taken off under the lock that found no
                        // repair wanted: a repair wanted after that starts
                        // a thread of its own.
                        if !std::mem::take(&mut wanted.0) {
                            wanted.1 = false;
                            return;
                        }
                    }
                    let r = router.repair();
                    fenec_http::log!("repair: {} {}", r.status, String::from_utf8_lossy(&r.body));
                }
            });
        if let Err(e) = spawned {
            self.repairs.lock().unwrap_or_else(|e| e.into_inner()).1 = false;
            fenec_http::log!("no thread for a repair: {e}");
        }
    }

    /// Runs a repair again in a while where this one passed over a tenant
    /// whose primary did not answer -- slow, or gone between its death and
    /// its lapse -- up to [`REPAIR_AGAIN_TRIES`] times, each wait twice the
    /// last; one that passed over none ends the count. The thread waiting
    /// holds the router weakly: a router let go of runs nothing again.
    fn repair_later(&self, unanswered: bool) {
        let mut again = self.again.lock().unwrap_or_else(|e| e.into_inner());
        if !unanswered {
            again.0 = 0;
            return;
        }
        if again.1 {
            return;
        }
        if again.0 >= REPAIR_AGAIN_TRIES {
            again.0 = 0;
            fenec_http::log!(
                "repair: a tenant's primary has not answered {REPAIR_AGAIN_TRIES} repairs in a \
                 row; POST /_shard/replicas gives it a replica once it does"
            );
            return;
        }
        let wait = (REPAIR_AGAIN_FIRST * 2u32.pow(again.0)).min(REPAIR_AGAIN_MOST);
        again.0 += 1;
        again.1 = true;
        let me = self.me.clone();
        let spawned = std::thread::Builder::new()
            .name("fenec-repair-later".into())
            .spawn(move || {
                std::thread::sleep(wait);
                if let Some(router) = me.upgrade() {
                    router.again.lock().unwrap_or_else(|e| e.into_inner()).1 = false;
                    router.repair_soon();
                }
            });
        if let Err(e) = spawned {
            again.1 = false;
            fenec_http::log!("no thread to run a repair again: {e}");
        }
    }

    /// Counts what the global entry's name for a request's client came to,
    /// and says in the log why one was not believed, a minute apart at
    /// most a kind: a token the entry does not share, or a clock a minute
    /// off, leaves every client behind the entry counted as Cloudflare's
    /// address, with nothing else to show for it. Said once a process, a
    /// router running for weeks said it once, long gone from any log kept.
    /// Not an audit event: the audit log's lines each cost their sender a
    /// refusal's wait, a token or a write, and a header that does not check
    /// costs nothing -- the request goes on as the connection's -- so a
    /// line each would let anyone grow the log at the router's request
    /// rate, and a token mistyped would write one for every request through
    /// the entry.
    fn entry_named(
        &self,
        named: std::result::Result<std::net::IpAddr, fenec_http::audit::Unnamed>,
        peer: Option<std::net::SocketAddr>,
    ) {
        use fenec_http::audit::Unnamed;
        use std::sync::atomic::AtomicU64;
        // The Unix second each kind was last said at; 0, never.
        static SAID: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
        let (how, said, why) = match named {
            Ok(_) => return metrics::entry_client(metrics::EntryClient::Named),
            Err(Unnamed::BadMark) => (
                metrics::EntryClient::BadMark,
                &SAID[0],
                "does not carry the global entry's mark for its address and time: is this \
                 router's --entry-token the entry's ENTRY_TOKEN? A client sending the header \
                 itself is answered the same",
            ),
            Err(Unnamed::Stale) => (
                metrics::EntryClient::Stale,
                &SAID[1],
                "carries the global entry's mark, made more than a minute from this router's \
                 clock: is either machine's clock off? A header replayed is answered the same",
            ),
        };
        metrics::entry_client(how);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let last = said.load(Ordering::Relaxed);
        if now >= last + 60
            && said
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            let from = peer.map_or_else(|| "?".to_string(), |p| p.to_string());
            fenec_http::log!(
                "a Fenec-Client header from {from} {why}; its client is not named, and the \
                 connection's address counts in its place (said a minute apart at most; \
                 fenec_router_entry_clients_total counts each)"
            );
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
    /// The router's own answer as it goes out: its CORS headers, and its
    /// mark ([`region::SHARD_HEADER`]).
    fn cors(&self, resp: Response) -> Response {
        fenec_http::cors::apply(resp, self.cfg.cors.as_deref()).header(region::SHARD_HEADER, "1")
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
            // The client the global entry named, believed with its mark
            // alone: from then on this request's in the connection's
            // place, the entry's address being Cloudflare's. Without
            // `--entry-token` the header is not looked for; with it, a
            // request without it costs one look among its headers, and a
            // tenant's request a count: an entry whose `ENTRY_TOKEN` is not
            // set names no one, and every client behind it would be
            // Cloudflare's address with nothing to show for it.
            if let (Some(key), None) = (&self.entry, relayed) {
                match fenec_http::audit::entry(&req, key) {
                    Some(named) => self.entry_named(named, peer),
                    None if req.path.starts_with("/t/") => {
                        metrics::entry_client(metrics::EntryClient::Absent)
                    }
                    None => {}
                }
            }
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
                Some(&"t") => {
                    // The client's socket, for a held request to watch: not
                    // where the reader holds bytes of its next request, a
                    // client that has gone having sent none (`upstream`'s
                    // `answered_or_gone`).
                    let watch = reader.buffer().is_empty().then(|| reader.get_ref());
                    self.forward(&req, &mut out, arrived, peer, relayed, watch)
                }
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
    /// came so ([`region`]); `watch` the client's socket, which a held
    /// request watches ([`upstream::Pool::send_held`]).
    fn forward(
        &self,
        req: &Request,
        out: &mut TcpStream,
        arrived: Instant,
        peer: Option<std::net::SocketAddr>,
        relayed: Option<std::net::IpAddr>,
        watch: Option<&TcpStream>,
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
        // A read its client marked (`Fenec-Read: replica`) goes to the
        // tenant's read replica in this router's region first, where it has
        // one ([`Self::forward_near`]); every other request asks the header
        // alone and goes on as before replicas. Kept out of this body: in it,
        // the path every unmarked request takes ran 0.4 to 0.65 us slower at
        // the median (`make shard-bench ARGS=latency`, ABBA turns against
        // main), the work the same.
        if matches!(fenec_http::read_marked(req), Ok(true)) {
            if let Some(kept) = self.forward_near(req, out, arrived, peer, relayed, tenant) {
                return kept;
            }
        }
        // The node holding the tenant, as this router's own directory has
        // it, with the node's region where the request asks for the
        // tenant's home ([`region::HOME_HEADER`]); or, where that says
        // nothing, the routers of the region the tenant lives in
        // ([`region`]).
        let placed = {
            let dir = self.read_dir();
            dir.placement(tenant).and_then(|p| {
                dir.node(&p.node).map(|n| {
                    let home = region::home_asked(req, &n.region);
                    (p.node.clone(), n.addr.clone(), self.mark(&n.token), home)
                })
            })
        };
        let mut home = None;
        let (node, addr, mark, routers) = match placed {
            Some((node, addr, mark, h)) => {
                home = h;
                (node, addr, Some(mark), None)
            }
            None => match self.elsewhere(tenant, relayed.is_some()) {
                region::Target::Node(node, addr, mark, region) => {
                    home = region::home_asked(req, &region);
                    (node, addr, Some(mark), None)
                }
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
        // own put in its place. The global entry's name for its client is
        // dropped too, believed or not, flag or not: it is this router's
        // to read, and the node takes the client from the router's.
        // Borrowed, not cloned: a request's headers were copied into
        // `String`s here and formatted one by one into the head, which with
        // the router's own header made a request through it 1.5 us slower.
        // A router relaying names the client it was relayed for, as the
        // router before it did, and the client the global entry named.
        let client = relayed
            .or_else(fenec_http::audit::named)
            .or(peer.map(|p| p.ip()));
        let named = client.zip(mark).map(|(ip, mark)| format!("{ip} {mark}"));
        let mut headers: Vec<(&str, &str)> = req
            .headers
            .iter()
            .filter(|(k, _)| {
                !hop_by_hop(k)
                    && !k.eq_ignore_ascii_case("host")
                    && !k.eq_ignore_ascii_case(fenec_http::audit::ROUTER_HEADER)
                    && !k.eq_ignore_ascii_case(fenec_http::audit::CLIENT_HEADER)
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
        // `Fenec-After` -- is waited for as long besides the timeout, and
        // watches its client meanwhile: one gone has the node's connection
        // let go of, so a claim's job goes to the next claim held.
        let held = req
            .header("fenec-wait")
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map_or(Duration::ZERO, |ms| {
                Duration::from_millis(ms).min(fenec_http::waits::LONGEST)
            });
        let watch = watch.filter(|_| !held.is_zero());
        let sending = match &routers {
            None => self.pool.send_held(
                &addr,
                req.method.name(),
                &req.target,
                &headers,
                &req.body,
                held,
                watch,
            ),
            // A region's routers, in turn from the one that answered last,
            // as far as the request cannot have run.
            Some((region, routers)) => self.relay_send(region, routers, req, &headers, held, watch),
        };
        let answer = match sending {
            Ok(a) => a,
            // Its client reset the connection: nobody is left to answer,
            // and the node is no more out of reach than before. Nothing is
            // written; the request is counted as `gone`, and the trace says
            // 499, as nginx and Envoy name a request its client closed. A
            // client gone with its end is not this: it reads as one that
            // half-closed, its end is passed on and the node's answer
            // written and counted as any (`upstream::answered_or_gone`).
            Err(e) if upstream::client_gone(&e) => {
                forwarding.error(e.to_string());
                drop(forwarding);
                metrics::gone(Route::Tenant, arrived.elapsed());
                fenec_http::trace::end(499);
                return false;
            }
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
                return reply(out, region::homed(Response::error(502, &why), &home));
            }
        };
        self.answer_out(
            req, out, arrived, peer, &node, answer, None, sent, forwarding, &home,
        )
    }

    /// The node's `answer` to `req`, forwarded to `node`, written to the
    /// client: its head as the router writes it, and its body -- `read`,
    /// where it was read already -- or stream. `home` the tenant's region
    /// where the request asked for it. Inlined
    /// where it is called, so the request every client sends compiles as it
    /// did written out in [`Self::forward`].
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    fn answer_out(
        &self,
        req: &Request,
        out: &mut TcpStream,
        arrived: Instant,
        peer: Option<std::net::SocketAddr>,
        node: &str,
        answer: upstream::Answer,
        read: Option<Vec<u8>>,
        sent: Instant,
        forwarding: fenec_http::trace::Span,
        home: &Option<String>,
    ) -> bool {
        let head_only = req.method == Method::Head;
        let reply = |out: &mut TcpStream, resp: Response| {
            let resp = self.cors(resp);
            let sent = resp.write(out, req.keep_alive, head_only).is_ok();
            metrics::request(Route::Tenant, resp.status, arrived.elapsed());
            fenec_http::trace::end(resp.status);
            sent && req.keep_alive
        };
        let status = answer.status;
        forwarding.attr("http.response.status_code", status);
        if status >= 500 {
            forwarding.error(status.to_string());
        }
        // Sized once and written into, never `format!`ed a header: grown
        // from the status line's length and a `format!` for each header the
        // node sent, it was allocated and copied again five times or so an
        // answer.
        let mut head = String::with_capacity(512);
        head.push_str("HTTP/1.1 ");
        http::push_number(&mut head, status as u64);
        head.push(' ');
        head.push_str(http::reason(status));
        head.push_str("\r\n");
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
                && !k.eq_ignore_ascii_case(region::SHARD_HEADER)
                && !(cors.is_some() && fenec_http::cors::is_cors_header(k, v))
            {
                for part in [k.as_str(), ": ", v.as_str(), "\r\n"] {
                    head.push_str(part);
                }
            }
        }
        if let Some(origin) = cors {
            fenec_http::cors::push_into(&mut head, origin);
        }
        if let Some(home) = home {
            head.push_str("Fenec-Home: ");
            head.push_str(home);
            head.push_str("\r\n");
        }
        head.push_str("Fenec-Shard: 1\r\n");
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
            let body = match read.map_or_else(|| answer.read_body(&self.pool, head_only), Ok) {
                Ok(b) => b,
                Err(e) => {
                    forwarding.error(e.to_string());
                    drop(forwarding);
                    metrics::unreachable(node);
                    let broke = Response::error(502, &format!("node `{node}` broke off: {e}"));
                    return reply(out, region::homed(broke, home));
                }
            };
            drop(forwarding);
            metrics::upstream(sent.elapsed());
            // A HEAD answer states the length of the body it leaves out.
            head.push_str("Content-Length: ");
            http::push_number(
                &mut head,
                (if head_only { length } else { body.len() }) as u64,
            );
            head.push_str(match req.keep_alive {
                true => "\r\nConnection: keep-alive\r\n\r\n",
                false => "\r\nConnection: close\r\n\r\n",
            });
            // One `writev`: the head and the body written apart went in two
            // packets, a send more every answer through the router.
            let written = http::write_parts(out, head.as_bytes(), &body);
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

    /// [`Self::forward`] of a read its client marked (`Fenec-Read:
    /// replica`): to the tenant's read replica in this router's region,
    /// where it has one ([`readers`]), and its answer written out. One the
    /// replica cannot answer -- not reached, or not by its `Fenec-After`'s
    /// wait and `--replica-timeout` past it; not caught up with its
    /// primary, or going down (503); behind the write its `Fenec-After`
    /// names past the wait (504); its file gone from the node (a 404 saying
    /// so) -- is `None`, and goes home after all, as an unmarked one does:
    /// the client is answered either way, and from no copy older than its
    /// write. A replica that did not answer is let be for a few seconds
    /// ([`readers::RESTING`]), its reads going home at once meanwhile, and
    /// one that answered 504 so for the reads sent after a position with
    /// no longer a wait. Cold: out of the way of the code every unmarked
    /// request runs.
    #[cold]
    #[inline(never)]
    fn forward_near(
        &self,
        req: &Request,
        out: &mut TcpStream,
        arrived: Instant,
        peer: Option<std::net::SocketAddr>,
        relayed: Option<std::net::IpAddr>,
        tenant: &str,
    ) -> Option<bool> {
        let waits = readers::after_wait(req);
        let near = match self.reader_near(tenant) {
            Some((rnode, ..))
                if self.resting(tenant, &rnode)
                    || waits.is_some_and(|w| self.behind(tenant, &rnode, w)) =>
            {
                metrics::replica_read(metrics::ReplicaRead::Fallback);
                None
            }
            Some(near) => Some(near),
            None => {
                metrics::replica_read(metrics::ReplicaRead::Home);
                None
            }
        };
        // The router's own context goes to the node in place of the
        // client's (`Pool::send`), while it traces; a router relaying
        // names the client it was relayed for, and the one the global entry
        // named.
        let traced = fenec_http::trace::propagating();
        let client = relayed
            .or_else(fenec_http::audit::named)
            .or(peer.map(|p| p.ip()));
        if let Some((rnode, raddr, rmark)) = near {
            let named = client.map(|ip| format!("{ip} {rmark}"));
            let mut headers = forwarded_headers(req, traced);
            if let Some(named) = &named {
                headers.push((fenec_http::audit::ROUTER_HEADER, named));
            }
            if fenec_http::trace::recording() {
                fenec_http::trace::attr("fenec.node", rnode.clone());
            }
            let sent = Instant::now();
            let forwarding = forward_span(req, &rnode, &raddr);
            let within = self.replica_within(req);
            match self.pool.send_within(
                &raddr,
                req.method.name(),
                &req.target,
                &headers,
                &req.body,
                within,
            ) {
                Ok(a) if a.status == 503 => {
                    forwarding.error("503");
                    metrics::replica_read(metrics::ReplicaRead::Fallback);
                }
                // Behind the position its read was sent after at the end of
                // the wait it was given: a copy following its primary waits
                // the wait out whatever the position -- it may yet be
                // pointed where that is -- so its 504 says it is behind
                // ([`Self::rest_behind`]). Asked again at each read, a
                // replica cut off from its primary held every one its whole
                // wait: each run of a live query reading near came that
                // late, for as long as the partition lasted.
                Ok(a) if a.status == 504 => {
                    forwarding.error("504");
                    if let Some(w) = waits {
                        self.rest_behind(tenant, &rnode, w);
                    }
                    metrics::replica_read(metrics::ReplicaRead::Fallback);
                }
                Ok(a) if a.status == 404 && a.header(fenec_http::ABSENT_HEADER).is_some() => {
                    forwarding.error("404");
                    self.reader_lost(tenant, &rnode);
                    metrics::replica_read(metrics::ReplicaRead::Fallback);
                }
                Ok(mut a) => {
                    // Read whole before a byte goes to the client, so one
                    // broken off goes home as one that failed its head
                    // does, where the client was answered 502. A stream
                    // has no length and is copied as it comes, its head
                    // gone on before its body: one cut short is the
                    // client's to see, as from home.
                    let body = match a.content_length() {
                        None => None,
                        Some(_) => match a.read_body_kept(&self.pool, req.method == Method::Head) {
                            Ok(body) => Some(body),
                            Err(e) => {
                                forwarding.error(e.to_string());
                                // Its connection let go of first: the
                                // question is the node's, on one of its own.
                                drop(a);
                                self.rest_unanswered(tenant, &rnode, &raddr);
                                metrics::replica_read(metrics::ReplicaRead::Fallback);
                                return None;
                            }
                        },
                    };
                    metrics::replica_read(metrics::ReplicaRead::Replica);
                    // A replica's region is not the tenant's: its answer
                    // names no home.
                    let home = None;
                    return Some(self.answer_out(
                        req, out, arrived, peer, &rnode, a, body, sent, forwarding, &home,
                    ));
                }
                // Not reached, or no whole request taken: the node's.
                Err(upstream::Near::Unreached(e)) => {
                    forwarding.error(e.to_string());
                    self.rest(&rnode);
                    metrics::unreachable(&rnode);
                    metrics::replica_read(metrics::ReplicaRead::Fallback);
                }
                // Taken and not answered in time, or its connection ended:
                // this tenant's copy at once -- its read maybe slower than
                // the timeout, or its query's panic dropping that
                // connection alone; resting the node sent every tenant's
                // marked reads of it home, and one slow read every few
                // seconds kept a region's replicas unread -- and the node
                // where its `/_health`, asked on a thread of its own, goes
                // unanswered too: one that takes connections and answers
                // nothing, stopped or deadlocked
                // (`readers::rest_unanswered`).
                Err(upstream::Near::Unanswered(e)) => {
                    forwarding.error(e.to_string());
                    self.rest_unanswered(tenant, &rnode, &raddr);
                    metrics::replica_read(metrics::ReplicaRead::Fallback);
                }
            }
        }

        None
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
    /// PUT    /_shard/tenants/<t>/read-replicas/<r>  a read replica of the
    ///                                       tenant in region r, which marked
    ///                                       reads near r go to ([`readers`]);
    ///                                       DELETE takes it off, ?forget its
    ///                                       record alone, its node gone
    /// GET    /_shard/tenants/<t>/read-replicas  {tenant, read_replicas:
    ///                                       [{region, node, follows}], failed,
    ///                                       changing}
    /// PUT    /_shard/tenants/<t>/traces/<r>   on the global router: region r,
    ///                                       keeping its own directory, may keep
    ///                                       a read replica of the name, which
    ///                                       a create of it asks r of; its
    ///                                       router says so as it places one,
    ///                                       and DELETE as its last goes. DELETE
    ///                                       by hand lets the name be made again
    ///                                       with r gone for good: a copy left
    ///                                       there takes nothing from another
    ///                                       tenant of the name (its lineage),
    ///                                       and deletes itself
    /// POST   /_shard/placements {tenants}     {"<t>": {node, addr, region,
    ///                                       quota, lineage} | null}: where this
    ///                                       directory places each, another
    ///                                       region's router asks
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
        ) && !readers::reads_only(req, &segs[1..]);
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
            // `{"force": true}` promotes each tenant's copy whatever it
            // holds: an operator's word, in the audit log.
            (Method::Post, ["nodes", n, "failover"]) => forced(&body).and_then(|force| {
                if force {
                    fenec_http::audit::event(
                        "failover_forced",
                        &[("node", fenec_http::audit::Field::Text(n))],
                    );
                }
                self.failover(n, &mut Vec::new(), force)
            }),
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
            (Method::Get, ["tenants", t, "read-replicas"]) => Ok(self.list_readers(t)),
            (Method::Put, ["tenants", t, "read-replicas", r]) => self.place_reader(t, r),
            (Method::Delete, ["tenants", t, "read-replicas", r]) => self.drop_reader(t, r, req),
            (Method::Put, ["tenants", t, "traces", r]) => self.set_trace(t, r),
            (Method::Delete, ["tenants", t, "traces", r]) => self.drop_trace(t, r),
            (Method::Post, ["placements"]) => Ok(self.placements(req)),
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
            history: None,
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
        // The entry's token is in a Worker's secrets: as a node's admin
        // token it would reach `/_admin/` there, every tenant's file.
        if self.cfg.entry_token.as_deref() == Some(token) {
            return Err(Fail(
                400,
                "the node's admin token is this router's --entry-token, which the global \
                 entry's Worker holds: give the node one of its own"
                    .into(),
            ));
        }
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
        let mut unanswered = false;
        for (tenant, primary) in wanting {
            // A copy attached while its primary does not answer follows
            // nothing it can take its first image from, and recorded as the
            // tenant's replica it was promotable: a failover of the primary
            // then moved the tenant onto a copy holding none of it, and the
            // repair after the primary came back had its copy follow that
            // one, the rows wiped. Its primary answering, it is asked again.
            if !holding.iter().any(|(node, _)| *node == primary) {
                unanswered = true;
                failed.push(why(
                    &tenant,
                    &format!(
                        "its primary `{primary}` does not answer, so a copy would follow nothing"
                    ),
                ));
                continue;
            }
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
        self.repair_later(unanswered);
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
    /// A tenant another operation holds is left, said, and added to `busy`:
    /// a failover by hand is answered at once, and one on a lease goes back
    /// to those once they are let go of ([`Self::fail_over_held`]).
    fn failover(&self, name: &str, busy: &mut Vec<HeldTenant>, force: bool) -> Outcome<Response> {
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
            return Ok(self.failover_replicas(name, busy, force));
        }
        let to = {
            let dir = self.read_dir();
            let s = dir
                .standby(name)
                .ok_or_else(|| Fail(409, format!("node `{name}` has no standby")))?
                .to_string();
            dir.node(&s)
                .ok_or_else(|| Fail(404, format!("no node `{s}`")))?;
            s
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
                failed.push(why(&tenant, "another operation is in progress"));
                busy.push(HeldTenant {
                    tenant,
                    to: Some(to.clone()),
                    own_replica: false,
                });
                continue;
            };
            match self.promote_on(&tenant, &to, &holder, false, force) {
                Ok(()) => promoted.push(quote(&tenant)),
                Err(e) => failed.push(why(&tenant, &e)),
            }
        }
        // Their read replicas follow the primaries they have now, from the
        // next look, which comes at once ([`readers`]).
        self.follow_soon();
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

    /// Promotes `tenant` on `to` -- its node's standby, or its own replica
    /// (`own_replica`, which then is its replica no more) -- and points the
    /// directory there: each tenant of a failover, its claim held. The node
    /// is told the tenant's lineage and the history its primary forked at
    /// its last failover, and refuses a copy that is not the tenant as its
    /// primary has it -- one that never took its primary's image, an old
    /// primary's copy that never heard from the new one -- unless `force`.
    fn promote_on(
        &self,
        tenant: &str,
        to: &str,
        holder: &str,
        own_replica: bool,
        force: bool,
    ) -> std::result::Result<(), String> {
        // Asked before each promotion: a router demoted since its failover
        // began -- its directory following another's -- promotes nothing,
        // where it went on until its next round noticed.
        let (node, lineage, history) = {
            let dir = self.read_dir();
            if dir.following() {
                return Err("this router follows another router now".into());
            }
            (
                dir.node(to).cloned(),
                dir.lineage(tenant),
                dir.history(tenant),
            )
        };
        let Some(node) = node else {
            return Err(format!("no node `{to}`"));
        };
        let mut said = Vec::new();
        for (key, value) in [("lineage", lineage), ("history", history)] {
            if let Some(v) = value {
                said.push(format!("{}:{}", quote(key), quote(&v)));
            }
        }
        if force {
            said.push("\"force\":true".into());
        }
        let body = format!("{{{}}}", said.join(","));
        let target = format!("/_admin/tenants/{tenant}/promote");
        let named = [(fenec_http::lease::HOLDER_HEADER, holder)];
        match self.pool.call_with(
            &node.addr,
            "POST",
            &target,
            &node.token,
            &named,
            body.as_bytes(),
        ) {
            Ok((200, body)) => {
                self.quota_after_failover(tenant, to);
                self.promoted(tenant, to, &body)
                    .and_then(|_| match own_replica {
                        true => self.write_dir().set_replica(tenant, None),
                        false => Ok(()),
                    })
                    .map_err(|e| message(&e))
            }
            Ok((status, body)) => Err(format!(
                "{status}: {}",
                String::from_utf8_lossy(&body).trim()
            )),
            Err(e) => Err(format!("`{to}` did not answer: {e}")),
        }
    }

    /// The failover of a node in no pair: each of its tenants promoted on
    /// its own replica and routed there, so its tenants spread over the
    /// nodes that held their replicas. One at a time, as a pair's are. The
    /// tenants whose replica it held have none now, and are named: a repair
    /// gives them one. A tenant another operation holds goes into `busy`,
    /// as in [`Self::failover`].
    fn failover_replicas(&self, name: &str, busy: &mut Vec<HeldTenant>, force: bool) -> Response {
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
                busy.push(HeldTenant {
                    tenant,
                    to: replica,
                    own_replica: true,
                });
                continue;
            };
            let Some(to) = replica else {
                failed.push(why(&tenant, "it has no replica"));
                continue;
            };
            match self.promote_on(&tenant, &to, &holder, true, force) {
                Ok(()) => promoted.push(format!(
                    "{{\"tenant\":{},\"to\":{}}}",
                    quote(&tenant),
                    quote(&to)
                )),
                Err(e) => failed.push(why(&tenant, &e)),
            }
        }
        self.follow_soon();
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
    /// standby and replica are sent it, so a failover finds it there, and
    /// its read replicas. A copy that does not answer is sent it again as
    /// it is promoted.
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
        let copies: Vec<String> = {
            let dir = self.read_dir();
            let own = [dir.standby(&node), dir.replica(tenant)];
            own.into_iter().flatten().map(str::to_string).collect()
        };
        for copy in copies {
            if let Some(w) = self.put_quota(tenant, &copy, &json) {
                fenec_http::log!("tenant `{tenant}`'s quota did not reach its copy: {w}");
            }
        }
        // Its read replicas too, each counting the streams and requests of
        // its region: this directory's handed to their nodes' senders now,
        // the request waiting on none of them, and every router keeping
        // one, another region's among them, at its next look, which reads
        // the quota where this directory keeps it ([`readers`]).
        let kept = (!set.is_empty()).then_some(json.as_str());
        let readers: Vec<String> = self
            .read_dir()
            .readers(tenant)
            .iter()
            .map(|r| r.node.clone())
            .collect();
        for reader in readers {
            self.want_reader_quota(tenant, &reader, kept);
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
        // And so is one a deleted tenant's read replicas still answer to.
        self.no_reader_traces(tenant)?;
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
        let made = expect(
            self.pool.call(&n.addr, "PUT", &target, &n.token, b""),
            201,
            &name,
        )?;
        // Its lineage beside its placement: its read replicas are bound to
        // it, and a tenant made again under the name has another.
        let lineage = lineage_in(&made);
        // Bound first: an `if let`'s guard lives through its block, the DELETE's round trip too.
        let placed = {
            let mut dir = self.write_dir();
            dir.place(tenant, &name, State::Active)
                .and_then(|_| dir.set_lineage(tenant, lineage.as_deref()))
        };
        if let Err(e) = placed {
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
        // Every change of its read replicas held until the tenant is gone,
        // one under way waited out -- before the claim, which a failover of
        // the tenant meanwhile would have found taken and passed it over.
        let _held = self.hold_readers(tenant)?;
        let _claim = self.claim(tenant)?;
        let placement = self
            .read_dir()
            .placement(tenant)
            .cloned()
            .ok_or_else(|| Fail(404, format!("no tenant `{tenant}` in the directory")))?;
        let n = self.node(&placement.node)?;
        let replica = self.read_dir().replica(tenant).map(str::to_string);
        // Its read replicas this directory keeps go first, the tenant still
        // placed: nothing routes a marked read to one once its record is
        // gone. One that stays refuses the delete, 502, the tenant kept.
        // Another directory's go at its router's next look, which finds
        // the tenant gone.
        self.take_readers(tenant)?;
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
        // So does a read replica in the region it moves into, which its own
        // copies there make no nearer -- and which may be on the target:
        // held until the directory places the tenant there, so that no
        // change of it lands on the node meanwhile.
        let into = dst.region.clone();
        let _arriving = self.readers_arriving(tenant, &into)?;

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

        // Bound first: an `if let`'s guard lives through its block, where `thaw` locks it again.
        let placed = self.write_dir().place(tenant, to, State::Active);
        if let Err(e) = placed {
            let _ = self.pool.call(&dst.addr, "DELETE", &base, &dst.token, b"");
            return Err(thaw(internal(e)));
        }
        self.lease_now(to);
        // Its read replicas follow it there, from the next look, which comes
        // at once: the image carried the history, so each goes on from where
        // it stood.
        self.follow_soon();

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
        self.router.freed.notify_all();
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

impl Router {
    /// Records `tenant` promoted on `to`, and the lineage its node says the
    /// promoted file is of (`"lineage"`): the tenant's own as a promotion
    /// keeps it -- or, another file of it promoted by force, one of its
    /// own, which the tenant is of from then on, the one before kept as one
    /// it was so that its read replicas are bound to it again rather than
    /// refuse it. A tenant whose lineage was not known is of the one said;
    /// a node from before lineages says none. Then the history the
    /// promotion forked (`"history"`), which the copy promoted at the next
    /// failover must hold.
    fn promoted(&self, tenant: &str, to: &str, answer: &[u8]) -> fenec_core::error::Result<()> {
        let fields =
            fenec_core::json::parse_object(&String::from_utf8_lossy(answer)).unwrap_or_default();
        let said = |key: &str| {
            fields.iter().find_map(|(k, v)| match v {
                Value::Text(t) if k == key => Some(t.clone()),
                _ => None,
            })
        };
        let mut dir = self.write_dir();
        // The lineage first, rarely another: the placement and the history
        // then go in one block, one sync, as the placement alone did.
        match (said("lineage"), dir.lineage(tenant)) {
            (Some(l), None) => dir.set_lineage(tenant, Some(&l))?,
            (Some(l), Some(known)) if known != l => dir.succeed_lineage(tenant, &l)?,
            _ => {}
        }
        dir.place_promoted(tenant, to, said("history").as_deref())
    }
}

/// The lineage a node's answer names (`"lineage": "<8 hex>"`): the one it
/// made a tenant of, or took one's image in at.
pub(crate) fn lineage_in(body: &[u8]) -> Option<String> {
    let fields = fenec_core::json::parse_object(&String::from_utf8_lossy(body)).ok()?;
    fields.into_iter().find_map(|(k, v)| match (k.as_str(), v) {
        ("lineage", Value::Text(l)) => Some(l),
        _ => None,
    })
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

/// The client's headers a forward sends on: not the connection's, not the
/// router's own name for its client -- what a client sent under it is
/// dropped, and the router's put in its place, which the node believes
/// from the router alone -- nor the global entry's, which is this
/// router's to read, nor the request id the router sends, nor,
/// while it traces, the client's trace context, which the router's own
/// replaces. Borrowed, not cloned: a request's headers were copied into
/// `String`s here and formatted one by one into the head, which with the
/// router's own header made a request through it 1.5 us slower.
fn forwarded_headers(req: &Request, traced: bool) -> Vec<(&str, &str)> {
    req.headers
        .iter()
        .filter(|(k, _)| {
            !hop_by_hop(k)
                && !k.eq_ignore_ascii_case("host")
                && !k.eq_ignore_ascii_case(fenec_http::audit::ROUTER_HEADER)
                && !k.eq_ignore_ascii_case(fenec_http::audit::CLIENT_HEADER)
                && !k.eq_ignore_ascii_case(fenec_http::request_id::HEADER)
                && !(traced
                    && (k.eq_ignore_ascii_case(fenec_http::trace::TRACEPARENT)
                        || k.eq_ignore_ascii_case(fenec_http::trace::TRACESTATE)))
        })
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect()
}

/// The span of a forward to `node` at `addr`, to its whole answer or a
/// stream's head: the node's server span is its child.
fn forward_span(req: &Request, node: &str, addr: &str) -> fenec_http::trace::Span {
    let forwarding = fenec_http::trace::span_of("forward", fenec_http::trace::Kind::Client);
    if fenec_http::trace::recording() {
        forwarding.attr("http.request.method", req.method.name());
        forwarding.attr("fenec.node", node.to_string());
        match addr.rsplit_once(':') {
            Some((host, port)) => {
                forwarding.attr("server.address", host.to_string());
                if let Ok(p) = port.parse::<i64>() {
                    forwarding.attr("server.port", p);
                }
            }
            None => forwarding.attr("server.address", addr.to_string()),
        }
    }
    forwarding
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

/// A failover's `"force"`: `true` promotes each copy whatever it holds.
fn forced(body: &[(String, Value)]) -> Outcome<bool> {
    match body.iter().find(|(k, _)| k == "force").map(|(_, v)| v) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(f)) => Ok(*f),
        Some(_) => Err(Fail(400, "`force` is true or false".into())),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// What a read sent to a read replica carries ([`Router::forward_near`]):
    /// the client's headers, less those that are the connection's or the
    /// router's to write -- the global entry's name for its client among
    /// them, which the router reads and the node never sees.
    #[test]
    fn a_replicas_read_is_sent_without_the_names_the_router_reads() {
        let header = |k: &str, v: &str| (k.to_string(), v.to_string());
        let req = Request {
            method: Method::Post,
            target: "/t/acme/query".into(),
            path: "/t/acme/query".into(),
            query: Vec::new(),
            headers: vec![
                header("Host", "router"),
                header("Authorization", "Bearer data"),
                header(
                    "Fenec-Client",
                    "203.0.113.7 1800000000 U6Z3YHsO6YiFX39RTKe6Fg",
                ),
                header("FENEC-ROUTER", "198.51.100.1 forged"),
                header("X-Request-Id", "r-1"),
                header("Fenec-Read", "replica"),
                header("Connection", "keep-alive"),
            ],
            body: Vec::new(),
            keep_alive: true,
        };
        let sent: Vec<&str> = forwarded_headers(&req, false)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(sent, ["Authorization", "Fenec-Read"]);
    }
}
