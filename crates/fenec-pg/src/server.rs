//! PostgreSQL-compatible session server.
//!
//! Any PostgreSQL client (psql, psycopg, node-postgres, JDBC) can connect to
//! fenecdb and run **FenecQL**. The language is not PostgreSQL SQL; what is
//! compatible is the *transport layer*. That lets existing connection pools,
//! proxies and tool chains be used unchanged.
//!
//! The session model:
//!
//! - One thread per connection.
//! - Read-only statements run *at the same time* under a shared lock; writes
//!   take the exclusive lock ([`Database::query`] / [`Database::execute_with`]).
//! - Writes are pushed to disk periodically by the background syncer; on a
//!   shutdown signal a final `sync` runs ([`SyncPolicy`]). After that -- when
//!   there is a vector index -- a checkpoint is written, otherwise every
//!   restart would rebuild the HNSW graph from scratch
//!   ([`Config::checkpoint_on_exit`]).
//! - `CancelRequest` is a real cancellation: the pending lock is released and
//!   the remaining statements are dropped with `57014`.
//! - A transaction's writes are one block ([`Database::begin`]): the write
//!   lock is taken at its first write and held until `COMMIT` lands it or
//!   `ROLLBACK` puts it back ([`Hold`]). A pipeline of the extended protocol
//!   is one block the same way, up to its `Sync`.

use crate::binary;
use crate::catalog;
use crate::compat;
use crate::copy;
use crate::params;
use crate::proto::*;
use crate::scram;
use crate::sql;
use fenec_core::json;
use fenec_core::prelude::*;
use fenec_core::query::projection_columns;
use fenec_core::value::DataType;
use fenec_http::metrics::Transport;
use fenec_http::tenants::{Refused, Tenant, Tenants};
use fenec_ql::parse;
use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError};
use std::time::{Duration, Instant};

static NEXT_PID: AtomicI32 = AtomicI32::new(1);

/// Whether a shutdown signal arrived. The signal handler writes only this
/// atomic; the syncer thread sees it and performs the final `sync`.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Sleep step for checking cancellation while waiting on a lock.
const LOCK_POLL: Duration = Duration::from_millis(1);

/// How often a session holding the write lock between messages looks up
/// from its wait for the client, to let go of it for a shutdown: the
/// shutdown takes that lock, and would otherwise wait on the client.
const HOLD_POLL: Duration = Duration::from_millis(200);

/// Tries at the lock that yield the thread before the waits turn into
/// `LOCK_POLL` sleeps. A lock held for a write's few microseconds is free
/// again long before a millisecond sleep ends; with eight writers and a
/// reader sleeping their way to it, the lock sat idle between holders.
const LOCK_SPINS: u32 = 64;

/// Wait after an accept error: a transient failure like EMFILE must not turn
/// into a hot loop.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// Stack of the session thread. `thread::spawn`'s default is 2 MiB; the
/// deepest accepted expression (`fenec_ql::MAX_EXPR_DEPTH` = 512) needs
/// ~750 KiB in release and ~5 MiB in a debug build. So the default is tight
/// in the first case (2.7x) and insufficient in the second -- and a stack
/// overflow is not a catchable panic but an `abort` of the process: a single
/// deep query would take the whole server down. 8 MiB is *virtual* space;
/// untouched pages are never resident (measured: RSS 5.2 MB over 100 idle
/// connections, i.e. ~36 KiB per connection -- independent of the stack size).
const SESSION_STACK: usize = 8 << 20;

/// After this many consecutive accept errors the listener is given up on.
/// A single successful accept resets the counter; the aim is to tolerate a
/// transient failure without turning a permanent one (EBADF, EINVAL) into an
/// infinite loop.
const ACCEPT_GIVE_UP: u32 = 64;

// ------------------------------------------------------------ configuration

/// The authentication method.
pub enum Auth {
    /// No authentication.
    Trust,
    /// The password is sent in plain text. Readable on the network without
    /// TLS; only for clients that cannot speak SCRAM.
    Cleartext(String),
    /// SCRAM-SHA-256: the password never travels the wire (recommended).
    Scram(Arc<scram::Verifier>),
}

impl Auth {
    pub fn parse(method: &str, password: &str) -> std::result::Result<Auth, String> {
        match method {
            "scram" | "scram-sha-256" => Ok(Auth::Scram(Arc::new(scram::Verifier::new(password)))),
            "cleartext" | "password" => Ok(Auth::Cleartext(password.to_string())),
            other => Err(format!("unknown authentication method: {other}")),
        }
    }

    fn is_trust(&self) -> bool {
        matches!(self, Auth::Trust)
    }
}

/// When writes are pushed to disk.
///
/// fenecdb does not `fsync` on every write; writes accumulate in a 1 MB
/// buffer. On the server side that has to be tied to a *policy*, otherwise
/// nothing reaches the disk until the buffer fills.
#[derive(Clone, Copy, PartialEq)]
pub enum SyncPolicy {
    /// Only on shutdown. The fastest, the least durable.
    Off,
    /// `fsync` after every write statement. The most durable, the slowest.
    Always,
    /// Periodic: at most one interval's worth of writes is at risk.
    Interval(Duration),
}

impl SyncPolicy {
    /// `off` | `always` | `<ms>`
    pub fn parse(s: &str) -> std::result::Result<SyncPolicy, String> {
        match s {
            "off" | "none" => Ok(SyncPolicy::Off),
            "always" | "on" => Ok(SyncPolicy::Always),
            ms => ms
                .parse::<u64>()
                .map(|n| SyncPolicy::Interval(Duration::from_millis(n)))
                .map_err(|_| format!("--sync expects off | always | <ms>, got `{ms}`")),
        }
    }
}

pub struct Config {
    pub addr: String,
    pub auth: Auth,
    /// When set, only this user name is accepted.
    pub user: Option<String>,
    pub server_version: String,
    pub sync: SyncPolicy,
    /// Since there is no TLS, binding openly outside loopback is refused by
    /// default; this flag removes that protection.
    pub insecure: bool,
    /// Rewrite the file image on shutdown. The HNSW graph lands in the file
    /// and the next open does not rebuild it (100k x 128: 4.4 s -> 9 ms).
    /// It is meaningless without a file (in-memory).
    pub checkpoint_on_exit: bool,
    /// Ceiling on concurrent connections (0 = unlimited). Every connection is
    /// an OS thread: leaving it unbounded hands the ceiling of stack memory
    /// to the client.
    pub max_connections: usize,
    /// A session that stays silent this long while waiting for the next
    /// message is closed (`None` = off). It applies mid-message too: both are
    /// signs of a dropped connection.
    pub idle_timeout: Option<Duration>,
    /// A transaction that has written holds the write lock until it ends,
    /// and every other session waits on it: one whose client stays silent
    /// this long is put back and its session closed (`25P03`, PostgreSQL's
    /// `idle_in_transaction_session_timeout`). `None` = never.
    pub idle_in_transaction: Option<Duration>,
    /// Ceiling of a single protocol message. The protocol allows up to 1 GiB;
    /// in a memory-limited container, letting one client force an allocation
    /// that large is a free OOM.
    pub max_message: usize,
    /// Data footprint ceiling (bytes, 0 = off). Above it, statements that
    /// *grow* the data are refused with `53200`.
    ///
    /// The point is to get ahead of the cgroup OOM: nobody warns you when
    /// `SIGKILL` lands, and both the final `sync` and the checkpoint are
    /// lost. The measured value is [`Database::memory_bytes`] -- not RSS, so
    /// pick it with headroom (`compact` peaks at ~3x the file).
    pub max_memory: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            addr: "127.0.0.1:5433".into(),
            auth: Auth::Trust,
            user: None,
            server_version: format!("16.0 (fenecdb {})", fenec_core::VERSION),
            sync: SyncPolicy::Interval(Duration::from_millis(250)),
            insecure: false,
            checkpoint_on_exit: true,
            max_connections: 100,
            idle_timeout: None,
            idle_in_transaction: Some(Duration::from_secs(10)),
            max_message: 64 << 20,
            max_memory: 0,
        }
    }
}

// ---------------------------------------------------------- cancellation

/// The cancellation state of a connection. Since `CancelRequest` arrives on
/// a separate TCP connection, the session state lives in a shared record.
struct Backend {
    secret: i32,
    /// Whether a query is being processed right now. Needed so that a
    /// cancellation landing in an idle moment does not kill the next
    /// innocent query.
    busy: AtomicBool,
    canceled: AtomicBool,
}

impl Backend {
    /// Consumes the flag when a cancellation was requested.
    fn take_cancel(&self) -> bool {
        self.canceled.swap(false, Ordering::SeqCst)
    }
}

type Backends = Arc<Mutex<HashMap<i32, Arc<Backend>>>>;

// ------------------------------------------------------------------ server

/// What a session runs against: one database, or a directory of tenants
/// where the startup packet's database name picks one (`fenec-pg --dir`).
#[derive(Clone)]
pub enum Source {
    One(Arc<RwLock<Database>>),
    Tenants(Arc<Tenants>),
}

impl Source {
    /// The database the next statement runs against, with the tenant it
    /// belongs to. A tenant is resolved again for every statement -- between
    /// two of them it can be frozen for a move, closed as idle or deleted --
    /// which is what the HTTP path does per request, for the same reason.
    fn open(&self, tenant: &str) -> std::result::Result<(Arc<RwLock<Database>>, Held), Refused> {
        match self {
            Source::One(db) => Ok((Arc::clone(db), None)),
            Source::Tenants(reg) => {
                let t = reg.get(tenant)?;
                Ok((Arc::clone(&t.db), Some(t)))
            }
        }
    }
}

/// The tenant a statement is running against, `None` over a single file.
type Held = Option<Arc<Tenant>>;

/// The registry's refusal as a client sees it: a name that is no tenant of
/// this node is PostgreSQL's unknown database, a node on its way down is its
/// "not accepting connections", and anything else a system error.
fn tenant_error(Refused(status, msg): Refused) -> (&'static str, String) {
    let code = match status {
        400 | 404 => "3D000",
        503 => "57P03",
        _ => "58000",
    };
    (code, msg)
}

pub struct Server {
    source: Source,
    cfg: Arc<Config>,
    backends: Backends,
    /// Number of live sessions. Incremented on accept, decremented via
    /// [`ConnGuard`] as the session thread ends.
    live: Arc<AtomicUsize>,
}

impl Server {
    pub fn new(db: Arc<RwLock<Database>>, cfg: Config) -> Server {
        Server::over(Source::One(db), cfg)
    }

    /// A server over a directory of tenants (`fenec-pg --dir`): the database
    /// name in the startup packet is the tenant. It runs no syncer of its
    /// own -- the registry's owner syncs and closes the files.
    pub fn with_tenants(tenants: Arc<Tenants>, cfg: Config) -> Server {
        Server::over(Source::Tenants(tenants), cfg)
    }

    fn over(source: Source, cfg: Config) -> Server {
        Server {
            source,
            cfg: Arc::new(cfg),
            backends: Arc::new(Mutex::new(HashMap::new())),
            live: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Starts listening and opens a thread per connection.
    pub fn serve(&self) -> io::Result<()> {
        let listener = self.bind()?;
        self.serve_on(listener)
    }

    /// Sets up the listener and performs the security checks.
    ///
    /// It stands apart from `serve` so the caller can learn the port
    /// *before* serving starts: binding `127.0.0.1:0` and reading
    /// `local_addr()` is safer than picking a fixed port in tests and in
    /// embedded use.
    pub fn bind(&self) -> io::Result<TcpListener> {
        let remote = is_remote(&self.cfg.addr);
        if remote && self.cfg.auth.is_trust() && !self.cfg.insecure {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} is not a loopback address and authentication is off.\n\
                     fenec-pg does not speak TLS; listening without auth on an \n\
                     open network exposes the whole database to everyone. Use \n\
                     SCRAM with `--password` (or --insecure if deliberate).",
                    self.cfg.addr
                ),
            ));
        }

        TcpListener::bind(&self.cfg.addr)
    }

    /// Serves on an already-prepared listener.
    pub fn serve_on(&self, listener: TcpListener) -> io::Result<()> {
        // The signal handlers come before the line saying it listens: a
        // supervisor that sends SIGTERM as soon as it reads that line would
        // otherwise stop it by the default action, with no final sync.
        install_signal_handlers();
        if let Source::One(db) = &self.source {
            spawn_syncer(Arc::clone(db), self.cfg.sync, self.cfg.checkpoint_on_exit);
        }
        fenec_http::log!(
            "fenec-pg {} listening on: postgres://localhost:{}/{}  [{}, sync={}]",
            fenec_core::VERSION,
            listener.local_addr()?.port(),
            match &self.source {
                Source::One(_) => "fenec",
                Source::Tenants(_) => "<tenant>",
            },
            match &self.cfg.auth {
                Auth::Trust => "no auth",
                Auth::Cleartext(_) => "password: plain text",
                Auth::Scram(_) => "password: SCRAM-SHA-256",
            },
            match self.cfg.sync {
                SyncPolicy::Off => "on shutdown".to_string(),
                SyncPolicy::Always => "every write".to_string(),
                SyncPolicy::Interval(d) => format!("{} ms", d.as_millis()),
            }
        );

        let mut failures = 0u32;
        for stream in listener.incoming() {
            // An accept error is not fatal on its own. There used to be a `?`
            // here: a single EMFILE (descriptor exhaustion) or a half-open
            // connection would take the whole server down. A transient error
            // is tolerated; a permanent one ends at ACCEPT_GIVE_UP.
            let mut stream = match stream {
                Ok(s) => {
                    failures = 0;
                    s
                }
                Err(e) => {
                    failures += 1;
                    fenec_http::log!("accept error ({failures}): {e}");
                    if failures >= ACCEPT_GIVE_UP {
                        return Err(e);
                    }
                    std::thread::sleep(ACCEPT_BACKOFF);
                    continue;
                }
            };
            // The counter is incremented on accept; as a single `fetch_add`
            // there is no race between the check and the increment. `Drop`
            // handles the decrement: whether the session ends normally or
            // panics, and even when `spawn` fails and drops the closure.
            let (guard, live) = ConnGuard::acquire(&self.live);
            if self.cfg.max_connections > 0 && live > self.cfg.max_connections {
                drop(guard);
                refuse(
                    &mut stream,
                    "53300",
                    &format!(
                        "too many connections (ceiling {})",
                        self.cfg.max_connections
                    ),
                );
                continue;
            }

            let source = self.source.clone();
            let cfg = Arc::clone(&self.cfg);
            let backends = Arc::clone(&self.backends);
            // A second descriptor for the refusal path: if `spawn` fails it
            // swallows the closure and with it the stream.
            let refused = stream.try_clone().ok();
            let spawned = std::thread::Builder::new()
                .name("fenec-pg session".to_string())
                .stack_size(SESSION_STACK)
                .spawn(move || {
                    let _guard = guard;
                    let _open = fenec_http::metrics::Connection::open(Transport::Pg);
                    let peer = stream
                        .peer_addr()
                        .map(|a| a.to_string())
                        .unwrap_or_default();
                    if let Err(e) = session(stream, source, cfg, backends) {
                        // Neither an idle connection that timed out nor a
                        // client that closed is noise.
                        let quiet = matches!(
                            e.kind(),
                            io::ErrorKind::UnexpectedEof
                                | io::ErrorKind::ConnectionReset
                                | io::ErrorKind::WouldBlock
                                | io::ErrorKind::TimedOut
                        );
                        if !quiet {
                            fenec_http::log!("session error ({peer}): {e}");
                        }
                    }
                });
            // The thread could not be created: RLIMIT_NPROC, the pids cgroup
            // or memory for the stack. `thread::spawn` panics in that case,
            // and the panic would land in the accept loop -- that is, in the
            // server itself. Now only that connection drops; the client gets
            // PostgreSQL's "too many clients" code.
            if let Err(e) = spawned {
                fenec_http::log!("could not create a thread: {e}");
                if let Some(mut s) = refused {
                    refuse(&mut s, "53300", "could not create a thread");
                }
            }
        }
        Ok(())
    }
}

/// Whether the address is open outside loopback.
fn is_remote(addr: &str) -> bool {
    match addr.to_socket_addrs() {
        Ok(mut it) => it.any(|a| !a.ip().is_loopback()),
        // An unresolvable address errors in bind anyway; do not block it here.
        Err(_) => false,
    }
}

// ---------------------------------------------------------------- durability

/// Catches `SIGINT`/`SIGTERM`. The handler only writes an atomic
/// (signal-safe); the real `sync` happens in the syncer thread.
///
/// Public for `fenec-pg --dir`, which has no pg listener and runs its own
/// syncer over the tenants.
pub fn install_signal_handlers() {
    // libc's `signal` function, declared directly so as not to add a
    // dependency. SIGINT=2, SIGTERM=15, SIGHUP=1.
    extern "C" {
        fn signal(sig: i32, handler: usize) -> usize;
    }
    extern "C" fn on_signal(_sig: i32) {
        SHUTDOWN.store(true, Ordering::SeqCst);
    }
    unsafe {
        for sig in [1, 2, 15] {
            signal(sig, on_signal as *const () as usize);
        }
    }
}

/// Whether a shutdown signal has arrived.
pub fn shutdown_requested() -> bool {
    SHUTDOWN.load(Ordering::SeqCst)
}

/// The flag a shutdown signal sets, for a thread of the server's that has
/// to stop with it: `--follow`'s follower stops at it, with every change it
/// applied on disk and confirmed to PostgreSQL.
pub fn shutdown_flag() -> &'static AtomicBool {
    &SHUTDOWN
}

/// What runs before the shutdown's sync and checkpoint.
type Before = Box<dyn FnOnce() + Send>;
static BEFORE_SHUTDOWN: std::sync::Mutex<Vec<Before>> = std::sync::Mutex::new(Vec::new());

/// Runs `f` once a shutdown signal has arrived, before the final sync and
/// checkpoint: the follower's thread is waited for there, so that the
/// checkpoint holds what it applied last.
pub fn before_shutdown(f: impl FnOnce() + Send + 'static) {
    BEFORE_SHUTDOWN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(Box::new(f));
}

/// The periodic syncer + the shutdown hook.
fn spawn_syncer(db: Arc<RwLock<Database>>, policy: SyncPolicy, checkpoint: bool) {
    let tick = match policy {
        SyncPolicy::Interval(d) if !d.is_zero() => d,
        // A loop is needed even with Off/Always, to notice the shutdown signal.
        _ => Duration::from_millis(200),
    };
    std::thread::spawn(move || loop {
        std::thread::sleep(tick);
        if SHUTDOWN.load(Ordering::SeqCst) {
            shutdown(&db, checkpoint);
        }
        if !matches!(policy, SyncPolicy::Interval(_)) {
            continue;
        }
        // No need to take the lock when nothing is dirty: idle passes must
        // not block the readers. After a storage error nothing is ever clean
        // again, and every pass would log the same error; the writes that
        // follow are refused by the engine and say it themselves.
        let pending = {
            let g = read_lock(&db);
            g.is_dirty() && g.failure().is_none()
        };
        // The flush takes the exclusive lock, the fsync does not: a reader
        // arriving while the disk works is not held up by it.
        if pending {
            let flushed = write_lock(&db).flush();
            match flushed {
                Ok(Some(durability)) => {
                    if let Err(e) = durability() {
                        fenec_http::log!("sync error: {e}");
                        write_lock(&db).fail(&e);
                    }
                }
                Ok(None) => {}
                Err(e) => fenec_http::log!("sync error: {e}"),
            }
        }
    });
}

/// The final `sync`, an optional checkpoint, exit.
///
/// The exclusive lock is *held until exit*. Releasing it before exiting
/// meant a write accepted between `sync` and `exit` could look successful to
/// the client and never reach the disk; the window was small but silent.
/// Sessions waiting on the lock do not wait for nothing: [`acquire`] sees
/// the shutdown flag and returns `57P01`.
fn shutdown(db: &RwLock<Database>, checkpoint: bool) -> ! {
    let before = std::mem::take(&mut *BEFORE_SHUTDOWN.lock().unwrap_or_else(|e| e.into_inner()));
    for f in before {
        f();
    }
    let mut g = write_lock(db);
    // A transaction still open between its statements is put back: its
    // writes never landed, and a checkpoint takes none over an open block.
    if g.in_block() {
        g.rejoin_block();
        g.rollback();
    }
    let dirty = g.is_dirty();
    if dirty {
        if let Err(e) = g.sync() {
            fenec_http::log!("sync error: {e}");
        }
    }
    fenec_http::log!(
        "\nshutting down: {}",
        if dirty {
            "writes were pushed to disk"
        } else {
            "no pending writes"
        }
    );

    // The checkpoint comes *after* the `sync` and only buys anything when
    // there is a vector index. Since the whole image is built in memory,
    // peak memory is ~3x the file and shutdown takes longer on a large
    // database; if we are killed meanwhile (docker stop timeout, cgroup
    // OOM) the data is already on disk and only the graph is lost, to be
    // rebuilt on open. Because `rewrite` writes to a side file and renames,
    // a half-written checkpoint cannot corrupt the file.
    if checkpoint && g.stats().iter().any(|s| !s.vector_indexes.is_empty()) {
        match g.checkpoint() {
            Ok(()) => fenec_http::log!("checkpoint written: the HNSW graph is persisted"),
            Err(e) => fenec_http::log!("could not write the checkpoint: {e}"),
        }
    }
    std::process::exit(0);
}

/// A read timeout. Unix reports `EAGAIN`, Windows `TimedOut`.
fn is_timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

fn read_lock(db: &RwLock<Database>) -> RwLockReadGuard<'_, Database> {
    db.read().unwrap_or_else(|e| e.into_inner())
}

fn write_lock(db: &RwLock<Database>) -> RwLockWriteGuard<'_, Database> {
    db.write().unwrap_or_else(|e| e.into_inner())
}

// -------------------------------------------------------------------- locks

/// Either a shared or an exclusive lock. Read-only statement sets run under
/// the shared lock, so reads do not block each other.
enum Guard<'a> {
    Read(RwLockReadGuard<'a, Database>),
    Write(RwLockWriteGuard<'a, Database>),
    /// A statement of the block the session holds open ([`Hold`]), which
    /// it joins.
    Turn(Turn<'a>),
}

impl Guard<'_> {
    fn db(&self) -> &Database {
        match self {
            Guard::Read(g) => g,
            Guard::Write(g) => g,
            Guard::Turn(t) => t,
        }
    }

    fn run(&mut self, stmt: &Statement, params: &[Value]) -> fenec_core::error::Result<Response> {
        match self {
            Guard::Read(g) => g.query(stmt, params),
            Guard::Write(g) => g.execute_with(stmt, params),
            Guard::Turn(t) => t.execute_with(stmt, params),
        }
    }

    /// A block over the write lock ([`Database::begin`]); a read needs none,
    /// and a held block's statement is the transaction's.
    fn begin(&mut self) -> fenec_core::error::Result<()> {
        match self {
            Guard::Write(g) => g.begin(),
            Guard::Read(_) | Guard::Turn(_) => Ok(()),
        }
    }

    fn commit(&mut self) -> fenec_core::error::Result<()> {
        match self {
            Guard::Write(g) => g.commit(),
            Guard::Read(_) | Guard::Turn(_) => Ok(()),
        }
    }

    fn rollback(&mut self) {
        if let Guard::Write(g) = self {
            g.rollback();
        }
    }

    /// Under `always`, hands the writes over before the answer is written,
    /// and returns what the answer has to wait for to be true. The error is
    /// returned rather than logged: a client told "done" for a write the
    /// disk refused would be believing a wrong answer.
    fn flush_if_needed(
        &mut self,
        policy: SyncPolicy,
    ) -> fenec_core::error::Result<Option<Durability>> {
        match self {
            Guard::Write(g) if policy == SyncPolicy::Always => g
                .flush()
                .inspect_err(|e| fenec_http::log!("sync error: {e}")),
            // A held block reaches the sink when it lands, not before.
            _ => Ok(None),
        }
    }
}

/// A `create index` or `compact` run beside the database. Not cancellable:
/// the build holds no lock a `CancelRequest` could release.
fn maintain(
    db: &Arc<RwLock<Database>>,
    cfg: &Config,
    stmt: &Statement,
    out: &mut Writer,
) -> Option<(Durability, Option<usize>)> {
    if let Some(msg) = fenec_http::over_ceiling(cfg.max_memory, &read_lock(db), stmt) {
        out.error("53200", &msg);
        return None;
    }
    if let Err(e) = Database::maintain(db, stmt).expect("a maintenance statement") {
        out.error(sqlstate(&e), &e.to_string());
        return None;
    }
    // The index's record waits for the disk under `always`, as any write
    // does; a compact's image was synced when it replaced the file.
    let durability = match cfg.sync {
        SyncPolicy::Always => match write_lock(db).flush() {
            Ok(d) => d,
            Err(e) => {
                out.error(sqlstate(&e), &e.to_string());
                return None;
            }
        },
        _ => None,
    };
    let answer = out.mark();
    out.command_complete(match stmt {
        Statement::Compact(_) => "VACUUM",
        _ => "OK",
    });
    durability.map(|d| (d, Some(answer)))
}

/// The SQLSTATE an engine error is reported under.
fn sqlstate(e: &Error) -> &'static str {
    match e {
        Error::NotFound(_) => "42P01",
        Error::Type(_) => "42804",
        Error::Query(_) => "42601",
        Error::Exists(_) => "42P07",
        // PostgreSQL's io_error: the disk refused, and the engine now refuses
        // writes until the file is reopened.
        Error::Io(_) => "58030",
        // read_only_sql_transaction: what a PostgreSQL standby answers, and
        // what pools and drivers look for to tell a replica from a primary.
        Error::ReadOnly(_) => "25006",
        Error::Denied(_) => "42501",
        _ => "XX000",
    }
}

/// Takes the lock in a *cancellable* way. A `CancelRequest` arriving while
/// waiting behind a long query is seen in this loop; otherwise a
/// cancellation would only take effect after the query finished.
///
/// A transaction another session holds open between its statements is not
/// the lock's to wait for: a read goes on over what has landed, the
/// block's writes put back while it waits ([`Database::park`]), and a
/// write waits for the transaction to end -- as it waited for the lock the
/// session held throughout, when it did.
fn acquire<'a>(db: &'a RwLock<Database>, write: bool, be: &Backend) -> Option<Guard<'a>> {
    wait_for(be, || {
        if write {
            let g = match db.try_write() {
                Ok(g) => g,
                Err(TryLockError::Poisoned(g)) => g.into_inner(),
                Err(TryLockError::WouldBlock) => return None,
            };
            return (!g.block_left()).then_some(Guard::Write(g));
        }
        let g = match db.try_read() {
            Ok(g) => g,
            Err(TryLockError::Poisoned(g)) => g.into_inner(),
            Err(TryLockError::WouldBlock) => return None,
        };
        if g.reads_landed() {
            return Some(Guard::Read(g));
        }
        drop(g);
        // Parked, the next try reads; one that cannot be parked is waited
        // for.
        match db.try_write() {
            Ok(mut w) => w.park(),
            Err(TryLockError::Poisoned(w)) => w.into_inner().park(),
            Err(TryLockError::WouldBlock) => false,
        };
        None
    })
}

/// Tries `take` until it gives, as cancellably as any lock: `None` for a
/// cancellation or a shutdown.
fn wait_for<T>(be: &Backend, mut take: impl FnMut() -> Option<T>) -> Option<T> {
    let mut tries = 0u32;
    loop {
        if let Some(t) = take() {
            return Some(t);
        }
        if be.take_cancel() {
            return None;
        }
        // Once shutdown has begun there is no point waiting for the lock:
        // the syncer will take the exclusive lock and exit shortly. The
        // waiting session is released here so shutdown is not delayed behind
        // the queue.
        if SHUTDOWN.load(Ordering::Relaxed) {
            return None;
        }
        tries += 1;
        if tries < LOCK_SPINS {
            std::thread::yield_now();
        } else {
            std::thread::sleep(LOCK_POLL);
        }
    }
}

/// The write lock the session takes for a statement of the block it holds
/// open ([`Hold`]): the block taken back for the statement
/// ([`Database::rejoin_block`]) and left again as the lock goes
/// ([`Database::leave_block`]), for readers to read beside it and writers
/// to wait for it.
struct Turn<'a>(RwLockWriteGuard<'a, Database>);

impl<'a> Turn<'a> {
    /// As cancellably as any lock.
    fn take(db: &'a RwLock<Database>, be: &Backend) -> Option<Turn<'a>> {
        wait_for(be, || match db.try_write() {
            Ok(g) => Some(Turn::of(g)),
            Err(TryLockError::Poisoned(g)) => Some(Turn::of(g.into_inner())),
            Err(TryLockError::WouldBlock) => None,
        })
    }

    /// For what cannot be refused: a block landed or put back.
    fn wait(db: &'a RwLock<Database>) -> Turn<'a> {
        Turn::of(write_lock(db))
    }

    fn of(mut g: RwLockWriteGuard<'a, Database>) -> Turn<'a> {
        g.rejoin_block();
        Turn(g)
    }
}

impl std::ops::Deref for Turn<'_> {
    type Target = Database;
    fn deref(&self) -> &Database {
        &self.0
    }
}

impl std::ops::DerefMut for Turn<'_> {
    fn deref_mut(&mut self) -> &mut Database {
        &mut self.0
    }
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        self.0.leave_block();
    }
}

// ------------------------------------------------------------- held locks

/// The block a session holds open between its messages: from a
/// transaction's first write to its end, or from a pipeline's first write
/// to its `Sync`. The session takes the write lock for each of its
/// statements alone ([`Turn`]) and leaves the block open between them
/// ([`Database::leave_block`]). Every other session's write waits for it
/// meanwhile -- nobody may write after the block's writes, since a block is
/// put back by cutting each store back to where it stood -- and every read
/// goes on, over what has landed ([`Database::park`]): held under the write
/// lock throughout, a transaction open 20 ms kept readers waiting up to
/// 34 ms.
struct Hold<'c> {
    db: &'c Arc<RwLock<Database>>,
    /// The tenant, kept from an idle close while its database is held.
    tenant: Held,
    /// Taken for a pipeline outside a transaction: it lands at the `Sync`.
    implicit: bool,
    /// Landed, or put back as it failed to: nothing left to put back.
    ended: bool,
}

impl Drop for Hold<'_> {
    /// Let go of any other way than by landing -- a failed statement, the
    /// client gone, a timeout, a shutdown, a panic -- the block is put back:
    /// whoever writes next would find it open, and wait for it forever.
    fn drop(&mut self) {
        if !self.ended {
            Turn::wait(self.db).rollback();
        }
    }
}

/// Where a session's statements find the database: under a lock each takes
/// for itself, or the one the session holds.
struct Lock<'c> {
    /// The database a hold is taken over, found once a pass of the
    /// session's loop: the hold borrows it for as long as it lasts, and the
    /// next pass finds it afresh, as a tenant's is found for each statement.
    found: &'c OnceCell<Arc<RwLock<Database>>>,
    hold: Option<Hold<'c>>,
}

impl<'c> Lock<'c> {
    /// The database the next statement runs against, and its tenant: the
    /// one held while there is one.
    fn open(
        &self,
        source: &Source,
        tenant: &str,
    ) -> std::result::Result<(Arc<RwLock<Database>>, Held), Refused> {
        match &self.hold {
            Some(h) => Ok((Arc::clone(h.db), h.tenant.clone())),
            None => source.open(tenant),
        }
    }

    /// Runs `f` over the database: the one held, with the block's writes
    /// in it, or `db` as it has landed.
    fn read<R>(&self, db: &RwLock<Database>, f: impl FnOnce(&Database) -> R) -> R {
        match &self.hold {
            Some(h) => {
                let mut t = Turn::wait(h.db);
                // A block that cannot be written again is put back by it,
                // and `f` reads what has landed.
                let _ = t.unpark();
                f(&t)
            }
            None => f(&fenec_http::held::read_landed(db)),
        }
    }

    /// Takes the write lock over `db`, as cancellably as any lock, and opens
    /// a block under it to hold between messages: the lock is the first
    /// statement's turn.
    fn take(
        &mut self,
        db: &Arc<RwLock<Database>>,
        tenant: &Held,
        be: &Backend,
        implicit: bool,
    ) -> std::result::Result<Turn<'c>, (&'static str, String)> {
        let cell: &'c OnceCell<_> = self.found;
        let found = cell.get_or_init(|| Arc::clone(db));
        debug_assert!(Arc::ptr_eq(found, db), "a pass holds one database");
        let mut guard = match acquire(found, true, be) {
            Some(Guard::Write(g)) => g,
            Some(_) => unreachable!("the write lock was asked for"),
            None if SHUTDOWN.load(Ordering::Relaxed) => {
                return Err(("57P01", "the server is shutting down".into()))
            }
            None => return Err(("57014", "the query was cancelled".into())),
        };
        guard.begin().map_err(|e| (sqlstate(&e), e.to_string()))?;
        self.hold = Some(Hold {
            db: found,
            tenant: tenant.clone(),
            implicit,
            ended: false,
        });
        Ok(Turn::of(guard))
    }

    /// Lands the held block and lets go of the lock. Under `always` its
    /// record is handed to the disk first, and what that returns is waited
    /// for once the lock is let go, as a lone statement's is.
    fn land(&mut self, cfg: &Config) -> fenec_core::error::Result<Option<Durability>> {
        let Some(mut h) = self.hold.take() else {
            return Ok(None);
        };
        let mut t = Turn::wait(h.db);
        // Landed, or put back by the engine as it failed to.
        h.ended = true;
        t.commit()?;
        match cfg.sync {
            SyncPolicy::Always => t.flush(),
            _ => Ok(None),
        }
    }
}

/// Lands a pipeline's block, at its `Sync`. Its statements' answers are
/// written already, as PostgreSQL writes them before the commit, so a
/// failure is an error of its own before the ReadyForQuery.
fn land_pipeline(lock: &mut Lock<'_>, cfg: &Config, out: &mut Writer) {
    let Some(db) = lock.hold.as_ref().map(|h| Arc::clone(h.db)) else {
        return;
    };
    match lock.land(cfg) {
        Ok(Some(durability)) => durable_or_refused(&db, durability, Some(out.mark()), out),
        Ok(None) => {}
        Err(e) => out.error(sqlstate(&e), &e.to_string()),
    }
}

/// Whether the session goes on after a COPY, or its connection is over.
#[derive(PartialEq)]
enum Copied {
    On,
    Close,
}

/// A COPY puts its rows this many at a time, or once the rows since the
/// last put came in [`COPY_BYTES`] of text: a put links its vectors on
/// every core. A put a row, as a driver's `executemany` sends them, loads
/// 100 000 128-dim rows at 5.3k rows/s with the graph kept, a COPY at
/// 17.2k (`make load-bench`).
const COPY_ROWS: usize = 10_000;
const COPY_BYTES: usize = 32 << 20;

/// A COPY, cancellable while it runs and counted as the one statement it
/// is, however many puts it made.
#[allow(clippy::too_many_arguments)]
fn run_copy(
    copying: std::result::Result<copy::Spec, copy::Refusal>,
    sql: &str,
    db: &Arc<RwLock<Database>>,
    held: &Held,
    cfg: &Config,
    be: &Backend,
    tx: &mut TxState,
    lock: &mut Lock<'_>,
    r: &mut BufReader<TcpStream>,
    w: &mut BufWriter<TcpStream>,
    out: &mut Writer,
    bounded: &mut bool,
    lands: bool,
) -> io::Result<Copied> {
    let (started, errors) = (Instant::now(), out.errors());
    be.busy.store(true, Ordering::SeqCst);
    be.canceled.store(false, Ordering::SeqCst);
    let copied = match copying {
        Err((code, msg)) => {
            out.error(code, &msg);
            Ok((Copied::On, 0))
        }
        Ok(spec) if spec.out => copy_out(spec, db, held, cfg, be, tx, lock, w, out, bounded),
        Ok(spec) => copy_in(
            spec, sql, db, held, cfg, be, tx, lock, r, w, out, bounded, lands,
        ),
    };
    be.busy.store(false, Ordering::SeqCst);
    be.canceled.store(false, Ordering::SeqCst);
    let (copied, n) = copied?;
    // A COPY that held the block read under its bound.
    r.get_ref().set_read_timeout(cfg.idle_timeout).ok();
    counted(sql, 0, started.elapsed(), out.errors() > errors, n);
    Ok(copied)
}

/// `COPY <collection> FROM STDIN`: the rows the client streams, put
/// [`COPY_ROWS`] at a time and all of them one block -- the transaction's,
/// or the COPY's own -- so an error, a bad row, a cancel or the client's
/// CopyFail puts back every row, as PostgreSQL's COPY is one command. Each
/// put's answer is taken back and one `COPY n` given. Once a put holds the
/// block, the lock is held between messages as a transaction's is, and
/// each wait for the client is bounded as its is. `lands`: a simple
/// query's COPY lands its block at the CopyDone; an Execute's is its
/// pipeline's, which lands at the Sync. An error is answered at once, as
/// PostgreSQL answers it: the session's loop drops what the client still
/// streams. Returns the rows copied.
#[allow(clippy::too_many_arguments)]
fn copy_in(
    spec: copy::Spec,
    sql: &str,
    db: &Arc<RwLock<Database>>,
    held: &Held,
    cfg: &Config,
    be: &Backend,
    tx: &mut TxState,
    lock: &mut Lock<'_>,
    r: &mut BufReader<TcpStream>,
    w: &mut BufWriter<TcpStream>,
    out: &mut Writer,
    bounded: &mut bool,
    lands: bool,
) -> io::Result<(Copied, u64)> {
    let errors = out.errors();
    // What would refuse every put is said before the client sends a row.
    let refused = if tx.failed {
        Some((
            "25P02",
            "current transaction is aborted, commands ignored until end of transaction block",
        ))
    } else if tx.open && tx.mode.read_only {
        Some((
            "25006",
            "cannot execute COPY FROM in a read-only transaction",
        ))
    } else if held.as_ref().is_some_and(|t| t.is_frozen()) {
        Some(("57P03", "the tenant is being moved; retry shortly"))
    } else {
        None
    };
    if let Some((code, msg)) = refused {
        out.error(code, msg);
        return Ok((Copied::On, 0));
    }
    let target = match lock.read(db, |d| copy::target(d, &spec)) {
        Ok(t) => t,
        Err((code, msg)) => {
            out.error(code, &msg);
            return Ok((Copied::On, 0));
        }
    };
    out.copy_in_response(target.columns.len(), spec.format == copy::Format::Binary);
    send(out, w, lock.hold.is_some(), cfg, bounded)?;

    let mut reader = copy::Reader::new(spec.format);
    let mut rows = Vec::new();
    let mut docs = Vec::new();
    let (mut done, mut bytes) = (0u64, 0usize);
    // Holding the block, every read from the client is bounded as a
    // transaction's wait for its next statement is -- the rest of a message
    // cut short too, which a bound on its first byte alone let hold the
    // database for as long as the client liked -- and a message already in
    // the buffer is read with no wait: the two timeouts set around each
    // wait were a system call each, 2% of a COPY without an index (119k ->
    // 121k rows/s).
    let limit = cfg.idle_in_transaction.or(cfg.idle_timeout);
    let idle = |holding: bool| match holding && cfg.idle_in_transaction.is_some() {
        true => (
            "25P03",
            "the COPY sat idle holding the database: it was put back, \
             and the connection is closing",
        ),
        false => ("57P05", "the session went idle, closing the connection"),
    };
    let mut read_bound = false;
    let (code, msg) = loop {
        if lock.hold.is_some() {
            if r.buffer().is_empty() {
                read_bound = false;
                let (code, msg) = match hold_wait(r, limit)? {
                    Waited::Ready => ("", ""),
                    Waited::Gone => return Ok((Copied::Close, 0)),
                    Waited::Shutdown => (
                        "57P01",
                        "the server is shutting down: the COPY was put back",
                    ),
                    Waited::Idle => idle(true),
                };
                if !code.is_empty() {
                    lock.hold = None;
                    out.error(code, msg);
                    let _ = send(out, w, false, cfg, bounded);
                    return Ok((Copied::Close, 0));
                }
            }
            if !read_bound {
                r.get_ref().set_read_timeout(limit).ok();
                read_bound = true;
            }
        }
        let m = match read_message_max(r, cfg.max_message) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok((Copied::Close, 0)),
            Err(e) => {
                let (code, msg) = match e.kind() {
                    _ if is_timeout(&e) => {
                        let (code, msg) = idle(lock.hold.is_some());
                        (code, msg.to_string())
                    }
                    io::ErrorKind::InvalidData => ("54000", e.to_string()),
                    _ => return Err(e),
                };
                lock.hold = None;
                out.error(code, &msg);
                let _ = out.flush_to(w);
                return Ok((Copied::Close, 0));
            }
        };
        match m.tag {
            b'd' => {
                if be.take_cancel() {
                    break ("57014", "the query was cancelled".to_string());
                }
                bytes += m.body.len();
                let read = reader
                    .feed(&m.body, &mut rows)
                    .and_then(|()| copy::documents(&target, &mut rows, &mut docs));
                if let Err(e) = read {
                    break e;
                }
                if docs.len() >= COPY_ROWS || bytes >= COPY_BYTES {
                    done += docs.len() as u64;
                    bytes = 0;
                    if !copy_put(
                        &target, &mut docs, true, sql, db, held, cfg, be, tx, lock, out,
                    ) {
                        return Ok((Copied::On, 0));
                    }
                }
            }
            // A Flush or a Sync means nothing during a COPY: a driver that
            // runs it through Execute sends its Sync right behind it, as
            // tokio-postgres does, and another after the CopyDone.
            b'H' | b'S' => {}
            b'c' => {
                let read = reader
                    .finish(&mut rows)
                    .and_then(|()| copy::documents(&target, &mut rows, &mut docs));
                if let Err(e) = read {
                    break e;
                }
                done += docs.len() as u64;
                if !copy_put(
                    &target, &mut docs, false, sql, db, held, cfg, be, tx, lock, out,
                ) {
                    return Ok((Copied::On, 0));
                }
                // The COPY's own block lands before it is answered.
                if lands && lock.hold.as_ref().is_some_and(|h| h.implicit) {
                    land_pipeline(lock, cfg, out);
                }
                if out.errors() > errors {
                    return Ok((Copied::On, 0));
                }
                out.command_complete(&format!("COPY {done}"));
                return Ok((Copied::On, done));
            }
            b'f' => {
                let why = take_cstr(&m.body, &mut 0);
                break ("57014", format!("COPY from stdin failed: {why}"));
            }
            other => {
                break (
                    "08P01",
                    format!("unexpected message type 0x{other:02X} during COPY from stdin"),
                )
            }
        }
    };
    out.error(code, &msg);
    Ok((Copied::On, 0))
}

/// How many rows a `COPY ... TO STDOUT` reads under one read lock before
/// it hands them to the client.
const COPY_OUT_ROWS: usize = 1_000;

/// `COPY <collection> TO STDOUT`: every row, in id order, a CopyData a row.
/// The rows are read a page at a time -- `where id > <the last>`, which
/// starts the scan where the page before stopped -- each page under a read
/// lock of its own and held against a move for its read alone, then written
/// to the client with no lock held: read whole under one, a slow client
/// kept every writer waiting while it took the rows, and the rows were all
/// in memory at once. So a row goes out once, as it stood when its page was
/// read, and a transaction that holds the database -- one that wrote, or
/// a serializable one -- reads it as it stands for the whole of the COPY.
/// 100 000 rows of a text, an int and a 128-dim vector go out at 157k
/// rows/s, PostgreSQL's own COPY TO at 156k, and in the binary format --
/// each cell as a binary query's, no number written out as text -- at
/// 1.54M rows/s against 747k (`make load-bench`, the median of three).
/// Returns the rows copied.
#[allow(clippy::too_many_arguments)]
fn copy_out(
    spec: copy::Spec,
    db: &Arc<RwLock<Database>>,
    held: &Held,
    cfg: &Config,
    be: &Backend,
    tx: &mut TxState,
    lock: &mut Lock<'_>,
    w: &mut BufWriter<TcpStream>,
    out: &mut Writer,
    bounded: &mut bool,
) -> io::Result<(Copied, u64)> {
    if tx.failed {
        out.error(
            "25P02",
            "current transaction is aborted, commands ignored until end of transaction block",
        );
        return Ok((Copied::On, 0));
    }
    if let Some(query) = &spec.query {
        return copy_query_out(query, &spec.format, db, held, lock, out);
    }
    let target = match lock.read(db, |d| copy::target(d, &spec)) {
        Ok(t) => t,
        Err((code, msg)) => {
            out.error(code, &msg);
            return Ok((Copied::On, 0));
        }
    };
    // The page's statement: the fields asked for, `id` coming from the row.
    let fields: Vec<&str> = target
        .columns
        .iter()
        .map(|(name, _)| name.as_str())
        .filter(|name| *name != "id")
        .collect();
    let select = match fields.is_empty() {
        true => String::new(),
        false => format!(" select {}", fields.join(", ")),
    };
    let page = format!(
        "get {}{select} where id > $1 limit {COPY_OUT_ROWS}",
        target.collection
    );
    let stmt = match read(&page) {
        Ok(mut s) if s.len() == 1 => s.remove(0),
        Ok(_) => unreachable!("one statement is written"),
        Err(e) => {
            out.error(sqlstate(&e), &e.to_string());
            return Ok((Copied::On, 0));
        }
    };
    let binary = spec.format == copy::Format::Binary;
    out.copy_out_response(target.columns.len(), binary);
    let mut line = Vec::new();
    // The binary header goes in the first row's CopyData, as PostgreSQL
    // sends it: psycopg reads a message a row, and took a message of the
    // header alone for a row cut short.
    let mut head: &[u8] = match binary {
        true => copy::BINARY_HEADER,
        false => &[],
    };
    if let copy::Format::Csv { header: true, .. } = spec.format {
        let names: Vec<String> = target.columns.iter().map(|(n, _)| n.clone()).collect();
        copy::header(&spec.format, &names, &mut line);
        out.copy_data(&line);
    }
    let (mut last, mut done) = (0u64, 0u64);
    let mut cells: Vec<Option<String>> = Vec::with_capacity(target.columns.len());
    loop {
        if be.take_cancel() {
            out.error("57014", "the query was cancelled");
            return Ok((Copied::On, 0));
        }
        let rows = {
            let _gate = held.as_ref().map(|t| t.enter());
            lock.read(db, |d| d.query(&stmt, &[Value::Int(last as i64)]))
        };
        let rows = match rows {
            Ok(r) => r,
            Err(e) => {
                out.error(sqlstate(&e), &e.to_string());
                return Ok((Copied::On, 0));
            }
        };
        let Some(set) = rows.rows() else {
            break;
        };
        for row in &set.rows {
            line.clear();
            line.extend_from_slice(std::mem::take(&mut head));
            let mut values = row.values.iter();
            if binary {
                // Each cell as its column is described, as a query's row
                // goes where Bind asks for binary: a list as its element's
                // array, a vector as pgvector sends one.
                line.extend_from_slice(&(target.columns.len() as i16).to_be_bytes());
                for (name, ty) in &target.columns {
                    let id;
                    let v = match name.as_str() {
                        "id" => {
                            id = Value::Int(row.id as i64);
                            &id
                        }
                        _ => values.next().unwrap_or(&Value::Null),
                    };
                    match binary::value(pg_oid(ty), v) {
                        Ok(Some(b)) => {
                            line.extend_from_slice(&(b.len() as i32).to_be_bytes());
                            line.extend_from_slice(&b);
                        }
                        Ok(None) => line.extend_from_slice(&(-1i32).to_be_bytes()),
                        Err(why) => {
                            out.error("0A000", &why);
                            return Ok((Copied::On, 0));
                        }
                    }
                }
            } else {
                cells.clear();
                for (name, _) in &target.columns {
                    cells.push(match name.as_str() {
                        "id" => Some(row.id.to_string()),
                        _ => values.next().and_then(to_pg_text),
                    });
                }
                copy::line(&spec.format, &cells, &mut line);
            }
            out.copy_data(&line);
            last = row.id;
        }
        done += set.rows.len() as u64;
        let full = set.rows.len() == COPY_OUT_ROWS;
        send(out, w, lock.hold.is_some(), cfg, bounded)?;
        if !full {
            break;
        }
    }
    if binary {
        // The trailer: a row of -1 columns.
        // With no row, the header rides with the trailer.
        let mut end = std::mem::take(&mut head).to_vec();
        end.extend_from_slice(&(-1i16).to_be_bytes());
        out.copy_data(&end);
    }
    out.copy_done();
    out.command_complete(&format!("COPY {done}"));
    Ok((Copied::On, done))
}

/// `COPY (<get>) TO STDOUT`: the rows of a FenecQL `get`, as the same `get`
/// answers them -- a `near`'s `_score` last, a `lookup`'s levels widened
/// into the row -- in the format asked for. The query is read whole under
/// one read lock, as it would be run on its own: its order, its page and
/// its ranking are the query's, not pages by id.
fn copy_query_out(
    query: &str,
    format: &copy::Format,
    db: &Arc<RwLock<Database>>,
    held: &Held,
    lock: &mut Lock<'_>,
    out: &mut Writer,
) -> io::Result<(Copied, u64)> {
    // A plain SELECT, as DuckDB reads a table, its pushed-down conditions'
    // literals read as their columns' types; FenecQL otherwise.
    let plain = sql::plain(query).filter(|p| p.columns.iter().all(|c| c.2.is_none()));
    let (text, params) = match &plain {
        Some(p) => {
            let (text, literals) = p.fenecql();
            let typed = lock.read(
                db,
                |d| -> std::result::Result<Vec<Value>, (&'static str, String)> {
                    let c = d
                        .collection(&p.collection)
                        .map_err(|e| (sqlstate(&e), e.to_string()))?;
                    literals
                        .iter()
                        .map(|(col, lit)| {
                            let ty =
                                match col.as_str() {
                                    "id" => DataType::Int,
                                    name => c.schema.field(name).map(|f| f.ty.clone()).ok_or_else(
                                        || ("42703", format!("column \"{name}\" does not exist")),
                                    )?,
                                };
                            copy::value(lit, &ty).map_err(|why| ("22P02", format!("{col}: {why}")))
                        })
                        .collect()
                },
            );
            match typed {
                Ok(params) => (text, params),
                Err((code, msg)) => {
                    out.error(code, &msg);
                    return Ok((Copied::On, 0));
                }
            }
        }
        None => (query.to_string(), Vec::new()),
    };
    let sel = match read(&text) {
        Ok(mut stmts) if stmts.len() == 1 && matches!(stmts[0], Statement::Select(_)) => {
            match stmts.remove(0) {
                Statement::Select(sel) => sel,
                _ => unreachable!("matched above"),
            }
        }
        Ok(_) => {
            out.error(
                "0A000",
                "COPY of a query takes one get: COPY (get ...) TO STDOUT",
            );
            return Ok((Copied::On, 0));
        }
        Err(e) => {
            out.error(sqlstate(&e), &e.to_string());
            return Ok((Copied::On, 0));
        }
    };
    let stmt = Statement::Select(sel);
    let answer = {
        let _gate = held.as_ref().map(|t| t.enter());
        lock.read(db, |d| {
            let cols = match &stmt {
                Statement::Select(sel) => select_columns(d, sel),
                _ => None,
            };
            d.query(&stmt, &params).map(|r| (r, cols))
        })
    };
    let (resp, cols) = match answer {
        Ok(a) => a,
        Err(e) => {
            out.error(sqlstate(&e), &e.to_string());
            return Ok((Copied::On, 0));
        }
    };
    let Some(rs) = resp.rows() else {
        out.error("0A000", "COPY of a query takes one get");
        return Ok((Copied::On, 0));
    };
    let rs = rs.flatten();
    let mut cols =
        cols.unwrap_or_else(|| rs.columns.iter().map(|c| (c.clone(), OID_TEXT)).collect());
    // A column cast to text goes as its text, in binary too: DuckDB asks a
    // vector so, having no reader for pgvector's type.
    if let Some(p) = &plain {
        for (col, (_, cast, _)) in cols.iter_mut().zip(&p.columns) {
            if *cast {
                col.1 = OID_TEXT;
            }
        }
    }
    let with_score = cols.last().is_some_and(|(n, _)| n == "_score");
    let binary = *format == copy::Format::Binary;
    out.copy_out_response(cols.len(), binary);
    let mut line = Vec::new();
    // The binary header goes in the first row's CopyData, as PostgreSQL
    // sends it: psycopg reads a message a row, and took a message of the
    // header alone for a row cut short.
    let mut head: &[u8] = match binary {
        true => copy::BINARY_HEADER,
        false => &[],
    };
    if let copy::Format::Csv { header: true, .. } = format {
        let names: Vec<String> = cols.iter().map(|(n, _)| n.clone()).collect();
        copy::header(format, &names, &mut line);
        out.copy_data(&line);
    }
    for row in &rs.rows {
        line.clear();
        line.extend_from_slice(std::mem::take(&mut head));
        let score = with_score.then_some(row.score);
        if binary {
            let cells = match binary_row(&cols, &row.values, score, &[1]) {
                Ok(c) => c,
                Err(why) => {
                    out.error("0A000", &why);
                    return Ok((Copied::On, 0));
                }
            };
            line.extend_from_slice(&(cells.len() as i16).to_be_bytes());
            for cell in &cells {
                match cell {
                    Some(b) => {
                        line.extend_from_slice(&(b.len() as i32).to_be_bytes());
                        line.extend_from_slice(b);
                    }
                    None => line.extend_from_slice(&(-1i32).to_be_bytes()),
                }
            }
        } else {
            let mut cells: Vec<Option<String>> = row.values.iter().map(to_pg_text).collect();
            if let Some(score) = score {
                cells.push(score.map(|s| format!("{s}")));
            }
            copy::line(format, &cells, &mut line);
        }
        out.copy_data(&line);
    }
    if binary {
        // With no row, the header rides with the trailer.
        let mut end = std::mem::take(&mut head).to_vec();
        end.extend_from_slice(&(-1i16).to_be_bytes());
        out.copy_data(&end);
    }
    let n = rs.rows.len() as u64;
    out.copy_done();
    out.command_complete(&format!("COPY {n}"));
    Ok((Copied::On, n))
}

/// A put of a COPY's rows so far, as a statement of its block -- which the
/// first put with `more` of them to come takes and holds, as a pipeline's
/// write does. `false` once it wrote its error.
#[allow(clippy::too_many_arguments)]
fn copy_put(
    target: &copy::Target,
    docs: &mut Vec<copy::Doc>,
    more: bool,
    sql: &str,
    db: &Arc<RwLock<Database>>,
    held: &Held,
    cfg: &Config,
    be: &Backend,
    tx: &mut TxState,
    lock: &mut Lock<'_>,
    out: &mut Writer,
) -> bool {
    if docs.is_empty() {
        return true;
    }
    let stmt = [Statement::Put {
        collection: target.collection.clone(),
        docs: std::mem::take(docs),
    }];
    // Held against a move for the put alone, as a transaction's statements
    // are: never across a wait for the client.
    let _gate = held.as_ref().map(|t| t.enter());
    let frozen = held.as_ref().is_some_and(|t| t.is_frozen());
    let (mark, errors) = (out.mark(), out.errors());
    let wait = run_locked(
        db,
        held,
        frozen,
        cfg,
        be,
        tx,
        lock,
        sql,
        Some(&stmt),
        &[],
        out,
        false,
        &[],
        more,
    );
    if let Some((durability, answer)) = wait {
        durable_or_refused(db, durability, answer, out);
    }
    if out.errors() > errors {
        return false;
    }
    // The COPY answers once, for every row.
    out.rewind(mark);
    true
}

/// How a wait for the client ended while the session held the lock.
enum Waited {
    Ready,
    Gone,
    Idle,
    Shutdown,
}

/// Waits for the client's next message while the session holds a block
/// open. Every other session's writes wait for it meanwhile, so the wait is
/// bounded by `limit`, and looks up every `HOLD_POLL` for a shutdown.
fn hold_wait(r: &mut BufReader<TcpStream>, limit: Option<Duration>) -> io::Result<Waited> {
    let deadline = limit.map(|l| Instant::now() + l);
    loop {
        if !r.buffer().is_empty() {
            return Ok(Waited::Ready);
        }
        if SHUTDOWN.load(Ordering::Relaxed) {
            return Ok(Waited::Shutdown);
        }
        let step = match deadline {
            Some(d) => match d.checked_duration_since(Instant::now()) {
                Some(left) if !left.is_zero() => left.min(HOLD_POLL),
                _ => return Ok(Waited::Idle),
            },
            None => HOLD_POLL,
        };
        r.get_ref().set_read_timeout(Some(step))?;
        match r.fill_buf() {
            Ok([]) => return Ok(Waited::Gone),
            Ok(_) => return Ok(Waited::Ready),
            Err(e) if is_timeout(&e) => {}
            Err(e) => return Err(e),
        }
    }
}

/// Writes the answers out. While the session holds the lock, a client that
/// stopped reading would keep it held, so the socket's writes are bounded
/// then as the wait for its next message is -- and only then: otherwise a
/// slow reader of a large answer holds up nobody.
fn send(
    out: &mut Writer,
    w: &mut BufWriter<TcpStream>,
    holding: bool,
    cfg: &Config,
    bounded: &mut bool,
) -> io::Result<()> {
    if holding != *bounded {
        let limit = if holding {
            cfg.idle_in_transaction
        } else {
            None
        };
        w.get_ref().set_write_timeout(limit)?;
        *bounded = holding;
    }
    out.flush_to(w)
}

// ------------------------------------------------------------------ session

#[derive(Default, Clone)]
struct Prepared {
    sql: String,
    /// The text read as FenecQL once, at `Parse`: a driver prepares a
    /// statement to bind and run it again and again, and read at each
    /// `Execute` the parser was a quarter of what the server did for a row
    /// by id. `None` for a text `compat` answers or one that does not
    /// parse, which `Execute` takes as it always did.
    parsed: Option<Arc<Vec<Statement>>>,
    /// The parameter types `Parse` named, 0 for one it left to the server.
    declared: Vec<i32>,
    /// Every parameter's type, as `Describe` reported them: what Bind
    /// reads a binary value by. Empty until then, and the `declared` ones
    /// are read by.
    types: Vec<i32>,
    /// The field type each parameter's place names, which Bind reads a
    /// text value as ([`params::places`]): found by `Describe`, or by the
    /// first Bind of a statement no `Describe` asked about.
    places: Option<Vec<Option<DataType>>>,
}

#[derive(Default, Clone)]
struct Portal {
    sql: String,
    parsed: Option<Arc<Vec<Statement>>>,
    stmt_name: String,
    params: Vec<Value>,
    /// Bind's result format codes: which columns go in binary.
    formats: Vec<i16>,
}

fn session(
    stream: TcpStream,
    source: Source,
    cfg: Arc<Config>,
    backends: Backends,
) -> io::Result<()> {
    stream.set_nodelay(true).ok();
    // This is the only way to close an idle session: the read timeout works
    // both while waiting for the next message and in the middle of a
    // half-received one -- both are signs of a dropped connection.
    stream.set_read_timeout(cfg.idle_timeout).ok();
    let peer_is_remote = stream
        .peer_addr()
        .map(|a| !a.ip().is_loopback())
        .unwrap_or(false);
    let mut r = BufReader::new(stream.try_clone()?);
    let mut w = BufWriter::new(stream);
    let mut out = Writer::new();

    // ---- startup: refuse SSL/GSSAPI, cancel request, then StartupMessage
    let params = loop {
        let len = read_i32(&mut r)?;
        if len < 8 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "short startup"));
        }
        // The ceiling comes *before the allocation*: since the body is sized
        // from `len`, leaving it unbounded meant an arbitrarily large
        // allocation before authentication.
        if len > MAX_STARTUP {
            out.error("54000", "the startup packet is too large");
            out.flush_to(&mut w)?;
            return Ok(());
        }
        let code = read_i32(&mut r)?;
        match code {
            SSL_REQUEST | GSSENC_REQUEST => {
                // Encryption is not supported: refuse with 'N', the client continues in plain.
                w.write_all(b"N")?;
                w.flush()?;
                continue;
            }
            CANCEL_REQUEST => {
                // Body: target pid + secret key.
                let pid = read_i32(&mut r)?;
                let secret = read_i32(&mut r)?;
                if let Some(be) = backends.lock().ok().and_then(|m| m.get(&pid).cloned()) {
                    // Ignore silently when the secret does not match (PostgreSQL does the same).
                    if be.secret == secret && be.busy.load(Ordering::SeqCst) {
                        be.canceled.store(true, Ordering::SeqCst);
                    }
                }
                return Ok(());
            }
            PROTOCOL_V3 => {
                let mut body = vec![0u8; (len - 8) as usize];
                r.read_exact(&mut body)?;
                let mut pos = 0;
                let mut map = HashMap::new();
                loop {
                    let k = take_cstr(&body, &mut pos);
                    if k.is_empty() {
                        break;
                    }
                    let v = take_cstr(&body, &mut pos);
                    map.insert(k, v);
                }
                break map;
            }
            other => {
                out.error("0A000", &format!("unsupported protocol {other}"));
                out.flush_to(&mut w)?;
                return Ok(());
            }
        }
    };

    // ---- authentication
    let user = params.get("user").cloned().unwrap_or_default();
    if user.is_empty() {
        out.error("28000", "the connection request has no user name");
        out.flush_to(&mut w)?;
        return Ok(());
    }
    if let Some(expected) = &cfg.user {
        if &user != expected {
            out.error("28000", &format!("user `{user}` is not accepted"));
            out.flush_to(&mut w)?;
            return Ok(());
        }
    }
    if let Err(msg) = authenticate(&cfg.auth, &user, &mut r, &mut w, &mut out) {
        out.error("28P01", &msg);
        out.flush_to(&mut w)?;
        return Ok(());
    }

    // ---- the tenant, over a directory of them: the database name.
    // Resolved once here so a name that is no tenant of this node fails at
    // connect, as PostgreSQL's unknown database does.
    let tenant = params.get("database").cloned().unwrap_or_default();
    if let Source::Tenants(reg) = &source {
        if let Err(refused) = reg.get(&tenant) {
            let (code, msg) = tenant_error(refused);
            out.error(code, &msg);
            out.flush_to(&mut w)?;
            return Ok(());
        }
    }
    // Whose statements this session's are counted as, and sees in
    // `pg_stat_statements`: a connection is a thread of its own.
    SCOPE.with(|s| {
        *s.borrow_mut() = matches!(source, Source::Tenants(_)).then(|| tenant.clone());
    });

    // ---- session record (for cancellation)
    let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);
    let secret = i32::from_le_bytes(
        crate::crypto::random_bytes(4)
            .try_into()
            .unwrap_or([0, 0, 0, 1]),
    );
    let be = Arc::new(Backend {
        secret,
        busy: AtomicBool::new(false),
        canceled: AtomicBool::new(false),
    });
    if let Ok(mut m) = backends.lock() {
        m.insert(pid, Arc::clone(&be));
    }
    let _guard = BackendGuard {
        pid,
        backends: Arc::clone(&backends),
    };

    out.auth_ok();
    out.parameter_status("server_version", &cfg.server_version);
    out.parameter_status("server_encoding", "UTF8");
    out.parameter_status("client_encoding", "UTF8");
    out.parameter_status("DateStyle", "ISO, MDY");
    out.parameter_status("TimeZone", "UTC");
    out.parameter_status("standard_conforming_strings", "on");
    out.parameter_status("integer_datetimes", "on");
    out.parameter_status("session_authorization", &user);
    out.parameter_status(
        "application_name",
        params
            .get("application_name")
            .map(|s| s.as_str())
            .unwrap_or(""),
    );
    out.backend_key_data(pid, secret);
    if peer_is_remote {
        // No TLS: a client connecting remotely should know.
        out.notice(
            "01000",
            "the connection is not encrypted: fenec-pg does not speak TLS, traffic is plain text",
        );
    }
    out.ready(b'I');
    out.flush_to(&mut w)?;

    // ---- main loop
    let mut prepared: HashMap<String, Prepared> = HashMap::new();
    let mut portals: HashMap<String, Portal> = HashMap::new();
    let mut described_stmts: HashSet<String> = HashSet::new();
    let mut described_portals: HashSet<String> = HashSet::new();
    let mut tx = TxState::default();
    // After an error in the extended protocol everything up to the next Sync
    // is read and dropped, as PostgreSQL does. A pipelining client has
    // written off what it queued behind the failure and reads no answer for
    // it: run anyway, a write it counted as aborted was made, and its
    // answers were read as the next query's.
    let mut skipping = false;
    // The error count before the extended message just handled, to tell
    // whether it failed. Checked at the top of the loop, which every arm
    // reaches -- a refusal `continue`s -- and before waiting on the client.
    let mut before: Option<u64> = None;
    // Whether the socket's writes are bounded, as they are while the
    // session holds the lock (`send`).
    let mut bounded = false;

    // A pass holds at most one lock between messages, over the database
    // found for it; the next pass finds its database afresh.
    loop {
        let found = OnceCell::new();
        let mut lock = Lock {
            found: &found,
            hold: None,
        };
        loop {
            if lock.hold.is_none() && found.get().is_some() {
                break;
            }
            if before.take().is_some_and(|n| out.errors() > n) {
                skipping = true;
                // Sent as it happens rather than at the Sync, as PostgreSQL
                // sends one: a client that waits on it before sending more
                // would otherwise wait for good.
                send(&mut out, &mut w, lock.hold.is_some(), &cfg, &mut bounded)?;
            }
            if lock.hold.is_some() {
                let limit = cfg.idle_in_transaction.or(cfg.idle_timeout);
                let waited = hold_wait(&mut r, limit)?;
                r.get_ref().set_read_timeout(cfg.idle_timeout).ok();
                let (code, msg) = match waited {
                    Waited::Ready => ("", ""),
                    Waited::Gone => return Ok(()),
                    Waited::Shutdown => (
                        "57P01",
                        "the server is shutting down: the transaction was put back",
                    ),
                    Waited::Idle if cfg.idle_in_transaction.is_some() => (
                        "25P03",
                        "the transaction sat idle holding the database: it was put back, \
                         and the connection is closing",
                    ),
                    Waited::Idle => ("57P05", "the session went idle, closing the connection"),
                };
                if !code.is_empty() {
                    lock.hold = None;
                    out.error(code, msg);
                    let _ = send(&mut out, &mut w, false, &cfg, &mut bounded);
                    return Ok(());
                }
            }
            let m = match read_message_max(&mut r, cfg.max_message) {
                Ok(m) => m,
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                // Timeout: the client is silent. We say why and close;
                // PostgreSQL's `idle_session_timeout` also uses 57P05.
                Err(e) if is_timeout(&e) => {
                    out.error("57P05", "the session went idle, closing the connection");
                    let _ = out.flush_to(&mut w);
                    return Ok(());
                }
                // A message over the ceiling: its body was never read, so the
                // stream is no longer in sync and closing is mandatory. Say why first.
                Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                    out.error("54000", &e.to_string());
                    let _ = out.flush_to(&mut w);
                    return Ok(());
                }
                Err(e) => return Err(e),
            };
            if skipping && !matches!(m.tag, b'S' | b'X') {
                continue;
            }
            if matches!(m.tag, b'P' | b'B' | b'D' | b'E' | b'C') {
                before = Some(out.errors());
            }

            match m.tag {
                // ------------------------------------------------ simple query
                b'Q' => {
                    let mut pos = 0;
                    let sql = take_cstr(&m.body, &mut pos);
                    let errors = out.errors();
                    let (db, held) = match lock.open(&source, &tenant) {
                        Ok(v) => v,
                        Err(refused) => {
                            let (code, msg) = tenant_error(refused);
                            out.error(code, &msg);
                            tx.settle(&mut lock, errors, &out);
                            out.ready(tx.status());
                            send(&mut out, &mut w, lock.hold.is_some(), &cfg, &mut bounded)?;
                            continue;
                        }
                    };
                    // A COPY's rows follow it, so it runs apart, its puts held
                    // against a move one at a time (`copy_in`).
                    if let Some(copying) = copy::parse(&sql) {
                        let copied = run_copy(
                            copying,
                            &sql,
                            &db,
                            &held,
                            &cfg,
                            &be,
                            &mut tx,
                            &mut lock,
                            &mut r,
                            &mut w,
                            &mut out,
                            &mut bounded,
                            true,
                        )?;
                        if copied == Copied::Close {
                            return Ok(());
                        }
                    } else {
                        // Held against a move for the length of the statement,
                        // as a request is held on the HTTP path -- and let go
                        // before the answer is written. The socket has no write
                        // timeout: a client that stopped reading a large answer
                        // held the tenant through the write, a freeze waited on
                        // it, and every request for the tenant queued behind the
                        // freeze.
                        let _gate = held.as_ref().map(|t| t.enter());
                        let frozen = held.as_ref().is_some_and(|t| t.is_frozen());
                        be.busy.store(true, Ordering::SeqCst);
                        be.canceled.store(false, Ordering::SeqCst);
                        // A text holding a BEGIN, a COMMIT, a savepoint or
                        // anything else answered here runs a statement at a
                        // time, as PostgreSQL runs a simple query: each in
                        // the transaction open, or in an implicit block --
                        // the pipeline's -- which a BEGIN takes into the
                        // transaction it opens, a COMMIT or a ROLLBACK
                        // closes, and the text's end lands; each answered,
                        // and none after the first error. Read whole, it was
                        // taken for FenecQL and refused, or -- `BEGIN ;
                        // ...` -- for its BEGIN alone. A text of FenecQL
                        // alone runs whole, one block, as it always did.
                        // A text with no `;` is one statement, as nearly
                        // every one is: split all the same, a `put` of
                        // 1 000 128-dim rows spent 1.8 ms of its 12.6 in
                        // the walk (79k -> 93k rows/s).
                        let pieces = match sql.contains(';') {
                            true => compat::statements(&sql),
                            false => Vec::new(),
                        };
                        let apart = pieces.len() > 1
                            && pieces
                                .iter()
                                .any(|p| compat::handle(p, &cfg, &|| false).is_some());
                        if !apart {
                            execute_into(
                                &db,
                                &held,
                                frozen,
                                &cfg,
                                &be,
                                &mut tx,
                                &mut lock,
                                &sql,
                                None,
                                &[],
                                &mut out,
                                false,
                                &[],
                                false,
                            );
                        } else if let Some((code, e)) = pieces.iter().find_map(|p| {
                            if copy::parse(p).is_some() {
                                return Some(("0A000", copy::ALONE.to_string()));
                            }
                            compat::handle(p, &cfg, &|| false)
                                .is_none()
                                .then(|| read(p).err())
                                .flatten()
                                .map(|e| ("42601", e.to_string()))
                        }) {
                            // PostgreSQL reads the whole text before it runs
                            // any of it: a statement it cannot read runs none.
                            out.error(code, &e);
                        } else {
                            for (i, piece) in pieces.iter().enumerate() {
                                let before = out.errors();
                                execute_into(
                                    &db,
                                    &held,
                                    frozen,
                                    &cfg,
                                    &be,
                                    &mut tx,
                                    &mut lock,
                                    piece,
                                    None,
                                    &[],
                                    &mut out,
                                    false,
                                    &[],
                                    i + 1 < pieces.len(),
                                );
                                if out.errors() > before {
                                    break;
                                }
                            }
                        }
                        be.busy.store(false, Ordering::SeqCst);
                        be.canceled.store(false, Ordering::SeqCst);
                    }
                    tx.settle(&mut lock, errors, &out);
                    // A pipeline left without its Sync ends at a simple query,
                    // its block landing as at one -- and so does a text's
                    // implicit block.
                    if lock.hold.as_ref().is_some_and(|h| h.implicit) {
                        land_pipeline(&mut lock, &cfg, &mut out);
                    }
                    drop(held);
                    drop(db);
                    out.ready(tx.status());
                    send(&mut out, &mut w, lock.hold.is_some(), &cfg, &mut bounded)?;
                }

                // ------------------------------------------------ extended
                b'P' => {
                    let mut pos = 0;
                    let name = take_cstr(&m.body, &mut pos);
                    let sql = take_cstr(&m.body, &mut pos);
                    described_stmts.remove(&name);
                    let text = sql.trim();
                    let parsed = compat::handle(text, &cfg, &|| false)
                        .is_none()
                        .then(|| read(text).ok().map(Arc::new))
                        .flatten();
                    let n = be_i16(&m.body, &mut pos).max(0);
                    let declared = (0..n).map(|_| be_i32(&m.body, &mut pos)).collect();
                    prepared.insert(
                        name,
                        Prepared {
                            sql,
                            parsed,
                            declared,
                            types: Vec::new(),
                            places: None,
                        },
                    );
                    out.parse_complete();
                }
                b'B' => {
                    let mut pos = 0;
                    let portal = take_cstr(&m.body, &mut pos);
                    let stmt = take_cstr(&m.body, &mut pos);
                    let (sql, parsed, types, places) = prepared
                        .get(&stmt)
                        .map(|p| {
                            let types = match p.types.is_empty() {
                                true => p.declared.clone(),
                                false => p.types.clone(),
                            };
                            (p.sql.clone(), p.parsed.clone(), types, p.places.clone())
                        })
                        .unwrap_or_default();

                    // parameter format codes
                    let nfmt = be_i16(&m.body, &mut pos);
                    let mut fmts = Vec::new();
                    for _ in 0..nfmt {
                        fmts.push(be_i16(&m.body, &mut pos));
                    }
                    // parameter values
                    let nparams = be_i16(&m.body, &mut pos);
                    // A value sent as text is read as the field its place
                    // names. A statement no `Describe` asked about --
                    // psycopg and node-postgres send `Parse` to `Execute` in
                    // one go -- has its places found at its first Bind.
                    let texts = (0..nparams as usize)
                        .any(|i| fmts.get(i).or(fmts.first()).copied().unwrap_or(0) != 1);
                    let places = match (places, &parsed) {
                        (None, Some(stmts)) if texts => {
                            be.busy.store(true, Ordering::SeqCst);
                            let found = places_of(&source, &tenant, &lock, &be, stmts);
                            be.busy.store(false, Ordering::SeqCst);
                            be.canceled.store(false, Ordering::SeqCst);
                            if let (Some(p), Some(prep)) = (&found, prepared.get_mut(&stmt)) {
                                prep.places = Some(p.clone());
                            }
                            found
                        }
                        (places, _) => places,
                    }
                    .unwrap_or_default();
                    let mut values = Vec::new();
                    let mut refused = None;
                    for i in 0..nparams {
                        let len = be_i32(&m.body, &mut pos);
                        if len < 0 {
                            values.push(Value::Null);
                            continue;
                        }
                        let raw = &m.body[pos..pos + len as usize];
                        pos += len as usize;
                        let binary =
                            fmts.get(i as usize).or(fmts.first()).copied().unwrap_or(0) == 1;
                        let oid = types.get(i as usize).copied().unwrap_or(0);
                        match params::decode(
                            raw,
                            binary,
                            oid,
                            places.get(i as usize).and_then(Option::as_ref),
                        ) {
                            Ok(v) => values.push(v),
                            Err((code, why)) => {
                                refused = Some((code, format!("bind parameter ${}: {why}", i + 1)));
                                break;
                            }
                        }
                    }
                    // Refused as PostgreSQL refuses a value its type's
                    // receive function will not read: the portal is not
                    // made, and a transaction fails.
                    if let Some((code, msg)) = refused {
                        let errors = out.errors();
                        out.error(code, &msg);
                        tx.settle(&mut lock, errors, &out);
                        continue;
                    }
                    // result format codes
                    let nres = be_i16(&m.body, &mut pos);
                    let formats = (0..nres).map(|_| be_i16(&m.body, &mut pos)).collect();
                    described_portals.remove(&portal);
                    portals.insert(
                        portal,
                        Portal {
                            sql,
                            parsed,
                            stmt_name: stmt,
                            params: values,
                            formats,
                        },
                    );
                    out.bind_complete();
                }
                // ------------------------------------------------ Describe
                b'D' => {
                    let mut pos = 0;
                    let kind = m.body.first().copied().unwrap_or(b'S');
                    pos += 1;
                    let name = take_cstr(&m.body, &mut pos);
                    // A statement's columns are described in text; a
                    // portal's in the formats its Bind asked for.
                    let (sql, parsed, formats, declared) = if kind == b'S' {
                        prepared.get(&name).map(|p| {
                            (
                                p.sql.clone(),
                                p.parsed.clone(),
                                Vec::new(),
                                p.declared.clone(),
                            )
                        })
                    } else {
                        portals.get(&name).map(|p| {
                            (
                                p.sql.clone(),
                                p.parsed.clone(),
                                p.formats.clone(),
                                Vec::new(),
                            )
                        })
                    }
                    .unwrap_or_default();
                    // An error here is answered as Execute answers one: the
                    // client's Sync brings the ReadyForQuery. Sent here as well,
                    // it made two for one Sync, and libpq read every answer after
                    // it one query late.
                    let errors = out.errors();
                    let (db, held) = match lock.open(&source, &tenant) {
                        Ok(v) => v,
                        Err(refused) => {
                            let (code, msg) = tenant_error(refused);
                            out.error(code, &msg);
                            tx.settle(&mut lock, errors, &out);
                            continue;
                        }
                    };
                    let _gate = held.as_ref().map(|t| t.enter());
                    be.busy.store(true, Ordering::SeqCst);
                    let shape = describe(
                        &db,
                        &cfg,
                        &sql,
                        parsed.as_deref().map(Vec::as_slice),
                        &declared,
                        &be,
                        &lock,
                    );
                    be.busy.store(false, Ordering::SeqCst);
                    be.canceled.store(false, Ordering::SeqCst);
                    let shape = match shape {
                        Some(s) => s,
                        None => {
                            out.error("57014", "the query was cancelled");
                            tx.settle(&mut lock, errors, &out);
                            continue;
                        }
                    };
                    if kind == b'S' {
                        out.parameter_description(&shape.params);
                        if let Some(p) = prepared.get_mut(&name) {
                            p.types = shape.params.clone();
                            p.places = Some(shape.places.clone());
                        }
                    }
                    match &shape.columns {
                        Some(cols) => {
                            out.row_description(cols, &formats);
                            if kind == b'S' {
                                described_stmts.insert(name);
                            } else {
                                described_portals.insert(name);
                            }
                        }
                        None => out.no_data(),
                    }
                }
                b'E' => {
                    let mut pos = 0;
                    let portal = take_cstr(&m.body, &mut pos);
                    // Borrowed, not cloned: its text, its name and every
                    // parameter -- a vector's components too -- were copied
                    // at each Execute.
                    let none = Portal::default();
                    let p = portals.get(&portal).unwrap_or(&none);
                    // If RowDescription was already sent with Describe we do not
                    // repeat it (the protocol says so); when Describe was skipped
                    // it is sent anyway, so the client is not left without column
                    // names.
                    let already = described_portals.contains(&portal)
                        || described_stmts.contains(&p.stmt_name);
                    let errors = out.errors();
                    let (db, held) = match lock.open(&source, &tenant) {
                        Ok(v) => v,
                        Err(refused) => {
                            let (code, msg) = tenant_error(refused);
                            out.error(code, &msg);
                            tx.settle(&mut lock, errors, &out);
                            // No ReadyForQuery here: the client sends Sync.
                            continue;
                        }
                    };
                    // More of the pipeline before its Sync: a write in it holds
                    // the lock to there, so the pipeline lands whole. The last
                    // statement before the Sync -- most often the only one --
                    // runs as a statement on its own does.
                    let pipeline = r.buffer().first() != Some(&b'S');
                    let copying = match &p.parsed {
                        Some(_) => None,
                        None => copy::parse(&p.sql),
                    };
                    if let Some(copying) = copying {
                        let copied = run_copy(
                            copying,
                            &p.sql,
                            &db,
                            &held,
                            &cfg,
                            &be,
                            &mut tx,
                            &mut lock,
                            &mut r,
                            &mut w,
                            &mut out,
                            &mut bounded,
                            false,
                        )?;
                        if copied == Copied::Close {
                            return Ok(());
                        }
                    } else {
                        let _gate = held.as_ref().map(|t| t.enter());
                        let frozen = held.as_ref().is_some_and(|t| t.is_frozen());
                        be.busy.store(true, Ordering::SeqCst);
                        be.canceled.store(false, Ordering::SeqCst);
                        execute_into(
                            &db,
                            &held,
                            frozen,
                            &cfg,
                            &be,
                            &mut tx,
                            &mut lock,
                            &p.sql,
                            p.parsed.as_deref().map(Vec::as_slice),
                            &p.params,
                            &mut out,
                            already,
                            &p.formats,
                            pipeline,
                        );
                        be.busy.store(false, Ordering::SeqCst);
                        be.canceled.store(false, Ordering::SeqCst);
                    }
                    tx.settle(&mut lock, errors, &out);
                }
                b'C' => {
                    let mut pos = 0;
                    let kind = m.body.first().copied().unwrap_or(b'S');
                    pos += 1;
                    let name = take_cstr(&m.body, &mut pos);
                    if kind == b'S' {
                        prepared.remove(&name);
                        described_stmts.remove(&name);
                    } else {
                        portals.remove(&name);
                        described_portals.remove(&name);
                    }
                    out.close_complete();
                }
                b'S' => {
                    skipping = false;
                    if lock.hold.as_ref().is_some_and(|h| h.implicit) {
                        land_pipeline(&mut lock, &cfg, &mut out);
                    }
                    out.ready(tx.status());
                    send(&mut out, &mut w, lock.hold.is_some(), &cfg, &mut bounded)?;
                }
                b'H' => {
                    send(&mut out, &mut w, lock.hold.is_some(), &cfg, &mut bounded)?;
                }
                // A transaction left open is put back as the lock is let go.
                b'X' => return Ok(()),
                // What a client still streams into a COPY answered with its
                // error, dropped as PostgreSQL drops it.
                b'd' | b'c' | b'f' => {}
                other => {
                    let errors = out.errors();
                    out.error("0A000", &format!("unsupported message `{}`", other as char));
                    tx.settle(&mut lock, errors, &out);
                    out.ready(tx.status());
                    send(&mut out, &mut w, lock.hold.is_some(), &cfg, &mut bounded)?;
                }
            }
        }
    }
}

/// Closes the connection with an `ErrorResponse`. PostgreSQL does the same:
/// the client sees the reason instead of "connection reset".
fn refuse(stream: &mut TcpStream, code: &str, msg: &str) {
    let mut out = Writer::new();
    out.error(code, msg);
    let _ = out.flush_to(stream);
}

/// Counter of live sessions. Incremented on accept, decremented in `Drop`.
struct ConnGuard(Arc<AtomicUsize>);

impl ConnGuard {
    /// Increments the counter and returns the new value.
    fn acquire(live: &Arc<AtomicUsize>) -> (ConnGuard, usize) {
        let n = live.fetch_add(1, Ordering::SeqCst) + 1;
        (ConnGuard(Arc::clone(live)), n)
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct BackendGuard {
    pid: i32,
    backends: Backends,
}

impl Drop for BackendGuard {
    fn drop(&mut self) {
        if let Ok(mut m) = self.backends.lock() {
            m.remove(&self.pid);
        }
    }
}

// ------------------------------------------------------------ authentication

fn authenticate(
    auth: &Auth,
    user: &str,
    r: &mut impl Read,
    w: &mut impl Write,
    out: &mut Writer,
) -> std::result::Result<(), String> {
    match auth {
        Auth::Trust => Ok(()),
        Auth::Cleartext(expected) => {
            out.auth_cleartext();
            out.flush_to(w).map_err(|e| e.to_string())?;
            let m = read_message(r).map_err(|e| e.to_string())?;
            let mut pos = 0;
            let got = take_cstr(&m.body, &mut pos);
            if m.tag != b'p' || !crate::crypto::ct_eq(got.as_bytes(), expected.as_bytes()) {
                return Err("password verification failed".into());
            }
            Ok(())
        }
        Auth::Scram(v) => {
            out.auth_sasl(&["SCRAM-SHA-256"]);
            out.flush_to(w).map_err(|e| e.to_string())?;

            // SASLInitialResponse: mechanism name + length + initial response
            let m = read_message(r).map_err(|e| e.to_string())?;
            if m.tag != b'p' {
                return Err("expected a SASL response".into());
            }
            let mut pos = 0;
            let mech = take_cstr(&m.body, &mut pos);
            if mech != "SCRAM-SHA-256" {
                return Err(format!("unsupported SASL mechanism: {mech}"));
            }
            let len = be_i32(&m.body, &mut pos);
            if len < 0 || pos + len as usize > m.body.len() {
                return Err("the initial SASL response is truncated".into());
            }
            let first = &m.body[pos..pos + len as usize];

            let mut ex = scram::Exchange::new(v);
            let server_first = ex.client_first(first)?;
            out.auth_sasl_continue(&server_first);
            out.flush_to(w).map_err(|e| e.to_string())?;

            let m = read_message(r).map_err(|e| e.to_string())?;
            if m.tag != b'p' {
                return Err("expected the final SASL response".into());
            }
            let server_final = ex.client_final(&m.body)?;
            out.auth_sasl_final(&server_final);
            let _ = user;
            Ok(())
        }
    }
}

fn be_i16(b: &[u8], pos: &mut usize) -> i16 {
    let v = i16::from_be_bytes([b[*pos], b[*pos + 1]]);
    *pos += 2;
    v
}

fn be_i32(b: &[u8], pos: &mut usize) -> i32 {
    let v = i32::from_be_bytes([b[*pos], b[*pos + 1], b[*pos + 2], b[*pos + 3]]);
    *pos += 4;
    v
}

/// A float as a PostgreSQL client writes it.
///
/// `num::parse_f64` is deliberately strict: FenecQL and JSON have no spelling
/// for infinity, so neither accepts one. PostgreSQL's `float8` input does,
/// and a client is entitled to send it, so the specials are recognised here
/// and every actual decimal goes to the shared parser. That way the server
/// stops linking `str::parse::<f64>()` -- and the 12 KB table of powers of
/// five behind it -- for the sake of the word `inf`.
///
/// The accepted set is exactly what `str::parse` took: an optional sign,
/// then `inf`, `infinity` or `nan`, case-insensitive, with nothing around
/// them. `-nan` keeps its sign bit, as negating a NaN does.
fn parse_float_param(t: &str) -> Option<f64> {
    let (neg, rest) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let special = if rest.eq_ignore_ascii_case("inf") || rest.eq_ignore_ascii_case("infinity") {
        f64::INFINITY
    } else if rest.eq_ignore_ascii_case("nan") {
        f64::NAN
    } else {
        // Not a special: hand the whole thing over, sign included.
        return fenec_core::num::parse_f64(t);
    };
    Some(if neg { -special } else { special })
}

/// Converts a PG parameter into a fenecdb value. A `[0.1,0.2]` arriving in
/// text format is recognised as an embedding (the same notation as pgvector).
pub(crate) fn decode_param(raw: &[u8], binary: bool) -> Value {
    if binary {
        return match raw.len() {
            8 => Value::Int(i64::from_be_bytes(raw.try_into().unwrap())),
            4 => Value::Int(i32::from_be_bytes(raw.try_into().unwrap()) as i64),
            _ => Value::Bytes(raw.to_vec()),
        };
    }
    let s = String::from_utf8_lossy(raw);
    let t = s.trim();
    if t.starts_with('[') {
        if let Ok(v) = json::parse(t) {
            return v;
        }
    }
    if let Ok(i) = t.parse::<i64>() {
        return Value::Int(i);
    }
    if let Some(f) = parse_float_param(t) {
        return Value::Float(f);
    }
    match t {
        "t" | "true" => Value::Bool(true),
        "f" | "false" => Value::Bool(false),
        _ => Value::Text(s.into_owned()),
    }
}

/// The type a field's column and parameter are described as: the one the
/// catalog shows it as, but a list, which goes as its text. A vector is
/// pgvector's -- sent as `text` before, it was a string to pgvector's own
/// clients, which register their codecs by the type's name.
pub(crate) fn pg_oid(ty: &DataType) -> i32 {
    match ty {
        DataType::Bool => OID_BOOL,
        DataType::Int => OID_INT8,
        DataType::Float => OID_FLOAT8,
        DataType::Bytes => OID_BYTEA,
        DataType::Timestamp => OID_TIMESTAMPTZ,
        DataType::Vector(_, VecPrec::F32) => binary::OID_VECTOR,
        DataType::Vector(_, VecPrec::F16) => binary::OID_HALFVEC,
        DataType::Sparse(_) => binary::OID_SPARSEVEC,
        // A list of one scalar type is its array, which a driver reads as a
        // list of its own; one of lists or vectors has no array type
        // PostgreSQL would read, and goes as its text.
        DataType::List(inner) => match **inner {
            DataType::Bool
            | DataType::Int
            | DataType::Float
            | DataType::Text
            | DataType::Bytes
            | DataType::Timestamp => binary::array_of(pg_oid(inner)),
            _ => OID_TEXT,
        },
        DataType::Text => OID_TEXT,
    }
}

/// Converts a fenecdb value into PostgreSQL's text representation.
pub fn to_pg_text(v: &Value) -> Option<String> {
    Some(match v {
        Value::Null => return None,
        Value::Bool(b) => (if *b { "t" } else { "f" }).to_string(),
        Value::Int(i) => i.to_string(),
        // PostgreSQL's own output format; client parsers can reject the
        // ISO-8601 form that uses `T`/`Z`.
        Value::Timestamp(ms) => fenec_core::time::format_pg(*ms),
        // What `{}` writes, from the browser module's writer: 52 ns a float
        // against `{}`'s 73, and 38 against 62 for a vector's `f32`s.
        Value::Float(f) => {
            let mut s = String::new();
            fenec_core::num::f64_into(&mut s, *f);
            s
        }
        Value::Text(s) => s.clone(),
        Value::Bytes(b) => {
            let mut s = String::from("\\x");
            for x in b {
                s.push_str(&format!("{x:02x}"));
            }
            s
        }
        // the same text notation as pgvector: [1,2,3]
        Value::Vector(v) => {
            let mut s = String::from("[");
            for (i, x) in v.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                fenec_core::num::f32_into(&mut s, *x);
            }
            s.push(']');
            s
        }
        // pgvector's sparsevec notation: {1:0.5,3:0.25}/30522
        Value::Sparse(dim, entries) => {
            let mut s = String::new();
            fenec_core::sparse::format_into(&mut s, *dim, entries);
            s
        }
        Value::List(items) => {
            // PostgreSQL's array notation, as `array_out` writes it: an
            // element quoted where it is empty, says NULL, or holds a brace,
            // a comma, a quote, a backslash or a space -- a timestamp's --
            // with each quote and backslash escaped. Only a text's quotes
            // were: a backslash in a text escaped the character after it
            // when read back, and bytes went unquoted, `\x01` read as `x01`.
            let mut s = String::from("{");
            for (i, x) in items.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                let Some(t) = to_pg_text(x) else {
                    s.push_str("NULL");
                    continue;
                };
                let quote = !matches!(x, Value::List(_))
                    && (t.is_empty()
                        || t.eq_ignore_ascii_case("null")
                        || t.bytes().any(|c| {
                            matches!(c, b'{' | b'}' | b',' | b'"' | b'\\')
                                || c.is_ascii_whitespace()
                        }));
                if !quote {
                    s.push_str(&t);
                    continue;
                }
                s.push('"');
                for c in t.chars() {
                    if c == '"' || c == '\\' {
                        s.push('\\');
                    }
                    s.push(c);
                }
                s.push('"');
            }
            s.push('}');
            s
        }
    })
}

/// The field type each parameter of `stmts` stands in the place of, read
/// under the lock as [`describe`] reads the schemas; `None` where the
/// database cannot be had or the wait is cancelled, and each value is read
/// by its look then, as it always was.
fn places_of(
    source: &Source,
    tenant: &str,
    lock: &Lock<'_>,
    be: &Backend,
    stmts: &[Statement],
) -> Option<Vec<Option<DataType>>> {
    let (db, held) = lock.open(source, tenant).ok()?;
    let _gate = held.as_ref().map(|t| t.enter());
    Some(match &lock.hold {
        Some(h) => {
            let mut t = Turn::take(h.db, be)?;
            let _ = t.unpark();
            params::places(&t, stmts)
        }
        None => params::places(acquire(&db, false, be)?.db(), stmts),
    })
}

/// The fixed columns of `collections` / `describe` output.
fn schema_columns() -> Vec<(String, i32)> {
    vec![
        ("collection".into(), OID_TEXT),
        ("field".into(), OID_TEXT),
        ("type".into(), OID_TEXT),
        ("index".into(), OID_TEXT),
    ]
}

/// The column list of a `Select` -- from the schema, without running the query.
fn select_columns(db: &Database, sel: &fenec_core::query::Select) -> Option<Vec<(String, i32)>> {
    let coll = db.collection(&sel.collection).ok()?;
    if sel.count {
        return Some(vec![(
            fenec_core::query::COUNT_COLUMN.to_string(),
            OID_INT8,
        )]);
    }
    if !sel.aggregate.is_empty() {
        let ty = |f: &str| coll.schema.field(f).map(|f| f.ty.clone());
        return sel
            .aggregate
            .iter()
            .map(|a| {
                let oid = match a {
                    Agg::Count => OID_INT8,
                    Agg::Avg(_) => OID_FLOAT8,
                    Agg::Sum(f) => match ty(f)? {
                        DataType::Int => OID_INT8,
                        _ => OID_FLOAT8,
                    },
                    Agg::Key(f) | Agg::Min(f) | Agg::Max(f) => pg_oid(&ty(f)?),
                };
                Some((a.label(), oid))
            })
            .collect();
    }
    let mut cols: Vec<(String, i32)> = projection_columns(&coll.schema, &sel.project)
        .into_iter()
        .map(|c| {
            let oid = coll
                .schema
                .field(&c)
                .map(|f| pg_oid(&f.ty))
                .unwrap_or(if c == "id" { OID_INT8 } else { OID_TEXT });
            (c, oid)
        })
        .collect();
    // The wire has no nested row, so `lookup` arrives flattened and its
    // columns have to be described here too. Falling through would not
    // error: the caller's fallback types every column it cannot place as
    // `text`, so an extended-protocol client would silently read every
    // child int and timestamp as a string -- and `Describe` answers before
    // the query runs, so nothing downstream could correct it.
    // One block per level, in the order `flatten` lays them out -- a chain
    // widens once per `lookup`, and a level left out here would be typed by
    // the caller's fallback rather than described.
    if let Some(l) = &sel.lookup {
        for step in l.chain() {
            let child = db.collection(&step.collection).ok()?;
            cols.extend(
                projection_columns(&child.schema, &step.project)
                    .into_iter()
                    .map(|c| {
                        let oid = child
                            .schema
                            .field(&c)
                            .map(|f| pg_oid(&f.ty))
                            .unwrap_or(if c == "id" { OID_INT8 } else { OID_TEXT });
                        (format!("{}.{}", step.collection, c), oid)
                    }),
            );
        }
    }
    // Every ranking carries its score: `near`, `match`, and what is built
    // on them (`rerank`, `fuse`).
    if sel.near.is_some() || sel.matcher.is_some() {
        cols.push(("_score".to_string(), OID_FLOAT8));
    }
    Some(cols)
}

/// `text` read as FenecQL, or as the plain `SELECT` of columns from one
/// collection it may be ([`sql::select`]); FenecQL's error otherwise.
fn read(text: &str) -> fenec_core::error::Result<Vec<Statement>> {
    parse(text).or_else(|e| match sql::select(text) {
        Some(q) => parse(&q),
        None => Err(e),
    })
}

/// A row whose cells are text -- the catalog's -- with the columns
/// `formats` asks for in binary sent in their type's binary format.
fn text_row(
    out: &mut Writer,
    cols: &[(String, i32)],
    row: &[Option<String>],
    formats: &[i16],
) -> std::result::Result<(), String> {
    if !binary::any_binary(formats) {
        out.data_row(row);
        return Ok(());
    }
    let cells = row
        .iter()
        .enumerate()
        .map(|(i, c)| match c {
            Some(s) if binary::binary_at(formats, i) => {
                binary::text(cols.get(i).map_or(OID_TEXT, |c| c.1), s).map(Some)
            }
            Some(s) => Ok(Some(s.clone().into_bytes())),
            None => Ok(None),
        })
        .collect::<std::result::Result<Vec<_>, String>>()?;
    out.data_row(&cells);
    Ok(())
}

/// A row of values, and its `_score` column when it has one, each column
/// in the format `formats` asks for.
fn binary_row(
    cols: &[(String, i32)],
    values: &[Value],
    score: Option<Option<f32>>,
    formats: &[i16],
) -> std::result::Result<Vec<Option<Vec<u8>>>, String> {
    let mut cells = Vec::with_capacity(values.len() + score.is_some() as usize);
    for (i, v) in values.iter().enumerate() {
        cells.push(match binary::binary_at(formats, i) {
            true => binary::value(cols.get(i).map_or(OID_TEXT, |c| c.1), v)?,
            false => to_pg_text(v).map(String::into_bytes),
        });
    }
    if let Some(score) = score {
        let binary = binary::binary_at(formats, values.len());
        cells.push(score.map(|s| match binary {
            true => (s as f64).to_be_bytes().to_vec(),
            false => format!("{s}").into_bytes(),
        }));
    }
    Ok(cells)
}

// ------------------------------------------------------------------ Describe

/// The `Describe` response: expected parameters and (when there are rows) the row format.
struct Shape {
    params: Vec<i32>,
    /// The field type each parameter's place names ([`params::places`]).
    places: Vec<Option<DataType>>,
    /// `None` -> `NoData`
    columns: Option<Vec<(String, i32)>>,
}

/// A catalog query run over the schemas as they stand: its columns with
/// their types, and its rows. One the catalog cannot read answers empty.
fn catalog_answer(
    db: &RwLock<Database>,
    lock: &Lock<'_>,
    cfg: &Config,
    sql: &str,
    params: &[Value],
) -> catalog::Answer {
    // The schemas are copied under the read lock and the query runs without
    // it: a catalog join is cheap, but a writer need not wait for one.
    let mut snap = lock.read(db, |d| {
        catalog::Snapshot::of(d, "fenec", &cfg.server_version)
    });
    // What the server counted, only for a query that reads it.
    if sql.contains("pg_stat_statements") {
        let rows = SCOPE.with(|s| fenec_http::statements::snapshot(view_of(&s.borrow())));
        let ms = |us: u64| us as f64 / 1000.0;
        snap = snap.with_statements(
            rows.into_iter()
                .map(|e| catalog::StatementRow {
                    queryid: e.id as i64,
                    query: e.text,
                    calls: e.calls as i64,
                    total_ms: ms(e.micros),
                    min_ms: ms(e.min_micros),
                    max_ms: ms(e.max_micros),
                    rows: e.rows as i64,
                })
                .collect(),
        );
    }
    catalog::answer(sql, params, &snap).unwrap_or_else(|_| catalog::Answer {
        columns: vec![("result".to_string(), OID_TEXT)],
        rows: Vec::new(),
    })
}

/// Works out the shape *without running* the query.
///
/// The previous version answered every Describe with `NoData` + an empty
/// parameter list; that misleads clients which learn parameter types from
/// the server (JDBC, psycopg's server-side binding mode). The columns can be
/// derived from the schema and the parameter count from the largest `$n` in
/// the statement.
/// `None` -> the query was cancelled while waiting for the lock.
fn describe(
    db: &RwLock<Database>,
    cfg: &Config,
    sql: &str,
    parsed: Option<&[Statement]>,
    declared: &[i32],
    be: &Backend,
    lock: &Lock<'_>,
) -> Option<Shape> {
    // A type the client named is the type; the rest are the server's.
    let named = |mut params: Vec<i32>| {
        for (i, t) in params.iter_mut().enumerate() {
            if let Some(&d) = declared.get(i).filter(|d| **d != 0) {
                *t = d;
            }
        }
        params
    };
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        return Some(Shape {
            params: Vec::new(),
            places: Vec::new(),
            columns: None,
        });
    }
    // Compatibility-layer queries are pure and fixed; the shape is read from
    // there. A statement parsed when it was prepared is none of them.
    let shim = match parsed {
        Some(_) => None,
        None => compat::handle(trimmed, cfg, &|| lock.read(db, |d| d.history().following)),
    };
    if let Some(shim) = shim {
        return Some(match shim {
            compat::Shim::Rows { columns, .. } => Shape {
                params: Vec::new(),
                places: Vec::new(),
                columns: Some(columns.into_iter().map(|c| (c, OID_TEXT)).collect()),
            },
            // A catalog query's columns do not depend on its parameters: it
            // is run with every one null to learn them.
            compat::Shim::Catalog => {
                // A parameter is the type the query casts it to: asyncpg
                // sends `$1::oid[]` as the array it is told.
                let types = catalog::param_types(trimmed).unwrap_or_default();
                let answer =
                    catalog_answer(db, lock, cfg, trimmed, &vec![Value::Null; types.len()]);
                Shape {
                    params: named(types),
                    places: Vec::new(),
                    columns: Some(answer.columns),
                }
            }
            // A refusal is reported by Execute, the way a syntax error is.
            compat::Shim::Tag(_) | compat::Shim::Tx(_) | compat::Shim::Refuse { .. } => Shape {
                params: Vec::new(),
                places: Vec::new(),
                columns: None,
            },
        });
    }
    if parsed.is_none() {
        if let Some((_, values)) = sql::constants(trimmed) {
            return Some(Shape {
                params: Vec::new(),
                places: Vec::new(),
                columns: Some(constant_columns(&values)),
            });
        }
    }
    let owned;
    let stmts: &[Statement] = match parsed {
        Some(s) => s,
        None => match read(trimmed) {
            Ok(s) => {
                owned = s;
                &owned
            }
            // A syntax error is reported during Execute; we do not branch
            // Describe off with a second error message.
            Err(_) => {
                return Some(Shape {
                    params: Vec::new(),
                    places: Vec::new(),
                    columns: None,
                })
            }
        },
    };
    let nparams = stmts.iter().map(|s| s.max_param()).max().unwrap_or(0);
    // The parameters' types and a select's columns come from the schemas.
    // When the collection does not exist yet we cannot know the shape;
    // rather than erroring we say NoData and let Execute speak.
    let of = |d: &Database| {
        let columns = match stmts.last() {
            Some(Statement::Select(sel)) => select_columns(d, sel),
            _ => None,
        };
        (params::places(d, stmts), columns)
    };
    let schemas = nparams > 0 || matches!(stmts.last(), Some(Statement::Select(_)));
    let (places, columns) = match (schemas, &lock.hold) {
        (false, _) => (Vec::new(), None),
        (true, Some(h)) => {
            let mut t = Turn::take(h.db, be)?;
            let _ = t.unpark();
            of(&t)
        }
        (true, None) => {
            // Reading the schema needs a shared lock; a cancellation
            // arriving while waiting behind a long write has to be seen
            // here too.
            let guard = acquire(db, false, be)?;
            of(guard.db())
        }
    };
    let columns = match stmts.last() {
        Some(Statement::ListCollections) | Some(Statement::Describe(_)) => Some(schema_columns()),
        Some(Statement::Explain(_)) => {
            Some(vec![(fenec_core::query::PLAN_COLUMN.to_string(), OID_TEXT)])
        }
        _ => columns,
    };
    let params = places
        .iter()
        .map(|t| t.as_ref().map_or(OID_TEXT, pg_oid))
        .collect();
    Some(Shape {
        params: named(params),
        places,
        columns,
    })
}

// ---------------------------------------------------------------- execution

/// A session's transaction, as PostgreSQL keeps one. Between `BEGIN` and
/// its end its writes are one block ([`Database::begin`]), which `COMMIT`
/// lands and `ROLLBACK` puts back -- as do a failed statement and the
/// session's end -- and `ROLLBACK TO` puts back as far as a savepoint. The
/// write lock is taken at its first write and held to its end ([`Hold`]),
/// so up to that write it reads what others have committed, statement by
/// statement -- PostgreSQL's read committed -- and from it on it runs
/// alone. `SERIALIZABLE` and `REPEATABLE READ` take the lock at its first
/// statement instead, so everything it reads is of one database, and
/// nothing it read changes before it ends.
#[derive(Default)]
struct TxState {
    open: bool,
    /// A statement in it failed, and every statement up to `COMMIT`,
    /// `ROLLBACK` or a `ROLLBACK TO` is refused, as PostgreSQL refuses them
    /// -- a client that went on would take the ones before the failure for
    /// landed. Its block is put back already, unless a savepoint keeps it
    /// ([`Self::keeps`]).
    failed: bool,
    /// Its savepoints, oldest first.
    savepoints: Vec<Point>,
    /// Whether a statement has run in it: its isolation is settled by then.
    ran: bool,
    mode: compat::Mode,
    /// Every transaction's mode (`SET SESSION CHARACTERISTICS`).
    default: compat::Mode,
}

/// A savepoint of a transaction.
struct Point {
    name: String,
    /// Where its block stood: the start, when it was taken before the
    /// transaction's first write.
    at: fenec_core::engine::Savepoint,
}

impl TxState {
    /// The ReadyForQuery status. libpq-based drivers such as psycopg 3 read
    /// their transaction state from it: `T` inside a transaction, `E` in one
    /// that failed, where they send `ROLLBACK` rather than more statements.
    fn status(&self) -> u8 {
        match (self.open, self.failed) {
            (false, _) => b'I',
            (true, false) => b'T',
            (true, true) => b'E',
        }
    }

    fn begin(&mut self, mode: compat::Mode) {
        *self = TxState {
            open: true,
            mode,
            default: self.default,
            ..TxState::default()
        };
    }

    fn end(&mut self) {
        *self = TxState {
            default: self.default,
            ..TxState::default()
        };
    }

    /// After a message: an error in a transaction fails it, and puts its
    /// block back now rather than at its end -- the others need not wait on
    /// a transaction that can only be rolled back -- and an error in a
    /// pipeline puts its block back before the `Sync` the rest of it is
    /// skipped to.
    fn settle(&mut self, lock: &mut Lock<'_>, errors: u64, out: &Writer) {
        if out.errors() > errors {
            if self.open {
                self.failed = true;
                if self.keeps() {
                    return;
                }
            }
            lock.hold = None;
        }
    }

    /// Whether a failed transaction's block and lock are kept for a
    /// `ROLLBACK TO`: a savepoint after one of its writes needs the writes
    /// before it, and a serializable transaction reads the database as it
    /// holds it. A savepoint before every write needs neither: taken back
    /// to, the transaction is as it was before its first.
    fn keeps(&self) -> bool {
        self.savepoints.iter().any(|p| !p.at.is_start())
            || (self.mode.serial && !self.savepoints.is_empty())
    }

    /// The newest savepoint named `name`, as PostgreSQL finds one.
    fn point(&self, name: &str, out: &mut Writer) -> Option<usize> {
        let i = self.savepoints.iter().rposition(|p| p.name == name);
        if i.is_none() {
            out.error("3B001", &format!("savepoint \"{name}\" does not exist"));
        }
        i
    }

    fn apply(
        &mut self,
        t: compat::Tx,
        lock: &mut Lock<'_>,
        cfg: &Config,
        out: &mut Writer,
    ) -> Option<(Durability, Option<usize>)> {
        match t {
            compat::Tx::Begin(change) => {
                if self.open {
                    out.notice("25001", "there is already a transaction in progress");
                } else {
                    self.begin(change.over(self.default));
                    // A pipeline's writes before it are the transaction's,
                    // as PostgreSQL makes them.
                    if let Some(h) = &mut lock.hold {
                        h.implicit = false;
                        self.ran = true;
                    }
                }
                out.command_complete("BEGIN");
                None
            }
            compat::Tx::Set(change) => {
                if !self.open {
                    out.notice(
                        "25P01",
                        "SET TRANSACTION can only be used in transaction blocks",
                    );
                } else if change.serial.is_some() && self.ran {
                    out.error(
                        "25001",
                        "SET TRANSACTION ISOLATION LEVEL must be called before any query",
                    );
                    return None;
                } else {
                    self.mode = change.over(self.mode);
                }
                out.command_complete("SET");
                None
            }
            compat::Tx::Default(change) => {
                self.default = change.over(self.default);
                out.command_complete("SET");
                None
            }
            compat::Tx::Commit { chain } => {
                let (open, failed, mode) = (self.open, self.failed, self.mode);
                if !open {
                    out.notice("25P01", "there is no transaction in progress");
                }
                self.end();
                if failed {
                    // What PostgreSQL answers a failed transaction's
                    // COMMIT: it was put back when it failed, or is now if
                    // a savepoint kept it.
                    lock.hold = None;
                    out.command_complete("ROLLBACK");
                    return None;
                }
                // Outside a transaction a pipeline's block lands here, as
                // PostgreSQL commits one at a COMMIT.
                let wait = match lock.land(cfg) {
                    Ok(d) => d,
                    Err(e) => {
                        out.error(sqlstate(&e), &e.to_string());
                        return None;
                    }
                };
                if open && chain {
                    self.begin(mode);
                }
                let answer = out.mark();
                out.command_complete("COMMIT");
                wait.map(|d| (d, Some(answer)))
            }
            compat::Tx::Rollback { chain } => {
                let (open, mode) = (self.open, self.mode);
                if !open {
                    out.notice("25P01", "there is no transaction in progress");
                }
                // Outside a transaction, a pipeline's block.
                lock.hold = None;
                self.end();
                if open && chain {
                    self.begin(mode);
                }
                out.command_complete("ROLLBACK");
                None
            }
            compat::Tx::Savepoint(name) => {
                if !self.open {
                    out.error("25P01", "SAVEPOINT can only be used in transaction blocks");
                    return None;
                }
                let at = match &lock.hold {
                    Some(h) => {
                        let mut t = Turn::wait(h.db);
                        match t.unpark() {
                            Ok(()) => t.savepoint(),
                            Err(e) => {
                                out.error(sqlstate(&e), &e.to_string());
                                return None;
                            }
                        }
                    }
                    None => fenec_core::engine::Savepoint::default(),
                };
                self.savepoints.push(Point { name, at });
                out.command_complete("SAVEPOINT");
                None
            }
            compat::Tx::Release(name) => {
                if !self.open {
                    out.error(
                        "25P01",
                        "RELEASE SAVEPOINT can only be used in transaction blocks",
                    );
                    return None;
                }
                let i = self.point(&name, out)?;
                self.savepoints.truncate(i);
                out.command_complete("RELEASE");
                None
            }
            compat::Tx::RollbackTo(name) => {
                if !self.open {
                    out.error(
                        "25P01",
                        "ROLLBACK TO SAVEPOINT can only be used in transaction blocks",
                    );
                    return None;
                }
                let i = self.point(&name, out)?;
                let p = &self.savepoints[i];
                if p.at.is_start() && !self.mode.serial {
                    // Before the first write: the lock goes with the writes,
                    // and up to its next write the transaction reads what
                    // others commit again.
                    lock.hold = None;
                } else if let Some(h) = &mut lock.hold {
                    if let Err(e) = Turn::wait(h.db).rollback_to(&p.at) {
                        out.error(sqlstate(&e), &e.to_string());
                        return None;
                    }
                }
                self.savepoints.truncate(i + 1);
                self.failed = false;
                out.command_complete("ROLLBACK");
                None
            }
        }
    }
}

/// Runs the query and writes the PG messages into `out`.
///
/// `row_desc_sent`: when the column description was already sent with
/// Describe it is not repeated.
///
/// Under `--sync always` the answer waits for the disk, but not under the
/// lock: [`run_locked`] hands the writes over and writes the answer, the lock
/// goes, and only then does the fsync run. Readers do not wait on the disk,
/// and writers that arrive meanwhile share the next fsync (see `FileSink`):
/// over eight clients on macOS, 268 durable writes/s became 1 156, and a
/// read's p99 under that load 320 ms became 0.44. A write the disk refused
/// has its answer taken back and replaced by the error.
///
/// `pipeline`: an Execute with more of its pipeline to come before the
/// `Sync`, which a write in it holds the lock to.
#[allow(clippy::too_many_arguments)]
fn execute_into(
    db: &Arc<RwLock<Database>>,
    tenant: &Held,
    frozen: bool,
    cfg: &Config,
    be: &Backend,
    tx: &mut TxState,
    lock: &mut Lock<'_>,
    sql: &str,
    parsed: Option<&[Statement]>,
    params: &[Value],
    out: &mut Writer,
    row_desc_sent: bool,
    formats: &[i16],
    pipeline: bool,
) {
    let started = Instant::now();
    let (errors, rows) = (out.errors(), out.rows());
    if parsed.is_none() {
        if let Some((count, values)) = sql::constants(sql) {
            constant_rows(
                db,
                tenant,
                lock,
                &count,
                &values,
                out,
                row_desc_sent,
                formats,
            );
            counted(
                sql,
                params.len(),
                started.elapsed(),
                out.errors() > errors,
                out.rows() - rows,
            );
            return;
        }
    }
    let wait = run_locked(
        db,
        tenant,
        frozen,
        cfg,
        be,
        tx,
        lock,
        sql,
        parsed,
        params,
        out,
        row_desc_sent,
        formats,
        pipeline,
    );
    if let Some((durability, answer)) = wait {
        durable_or_refused(db, durability, answer, out);
    }
    counted(
        sql,
        params.len(),
        started.elapsed(),
        out.errors() > errors,
        out.rows() - rows,
    );
}

/// The columns a `SELECT 1 FROM t` answers: a constant a column, a whole
/// number as `bigint` and anything else as text.
fn constant_columns(values: &[String]) -> Vec<(String, i32)> {
    values
        .iter()
        .map(|v| {
            let oid = if v.parse::<i64>().is_ok() {
                OID_INT8
            } else {
                OID_TEXT
            };
            ("?column?".to_string(), oid)
        })
        .collect()
}

/// `SELECT 1 FROM t [WHERE ...]`, as Spark counts a table's rows: the
/// matching rows counted, and as many rows of the constants written. It
/// was a FenecQL syntax error, and a Spark `count()` a failed job; a row's
/// id in place of the constant would have counted as well, and been a
/// wrong answer to a question nobody reads the answer to until someone
/// does.
#[allow(clippy::too_many_arguments)]
fn constant_rows(
    db: &Arc<RwLock<Database>>,
    tenant: &Held,
    lock: &mut Lock<'_>,
    count: &str,
    values: &[String],
    out: &mut Writer,
    row_desc_sent: bool,
    formats: &[i16],
) {
    let stmt = match read(count) {
        Ok(mut s) if s.len() == 1 => s.remove(0),
        Ok(_) => return out.error("42601", "one statement is expected"),
        Err(e) => return out.error(sqlstate(&e), &e.to_string()),
    };
    let answer = {
        let _gate = tenant.as_ref().map(|t| t.enter());
        lock.read(db, |d| d.query(&stmt, &[]))
    };
    let n = match answer.as_ref().map(|r| {
        r.rows()
            .and_then(|rs| rs.rows.first())
            .map(|row| &row.values[..])
    }) {
        Ok(Some([Value::Int(n)])) => (*n).max(0) as u64,
        Ok(_) => return out.error("XX000", "a count answered no number"),
        Err(e) => return out.error(sqlstate(e), &e.to_string()),
    };
    let cols = constant_columns(values);
    if !row_desc_sent {
        out.row_description(&cols, formats);
    }
    let row: Vec<Value> = values
        .iter()
        .map(|v| {
            v.parse::<i64>()
                .map_or_else(|_| Value::Text(v.clone()), Value::Int)
        })
        .collect();
    let cells: Vec<Option<Vec<u8>>> = match binary_row(&cols, &row, None, formats) {
        Ok(c) => c,
        Err(e) => return out.error("0A000", &e),
    };
    for _ in 0..n {
        out.data_row(&cells);
    }
    out.command_complete(&format!("SELECT {n}"));
}

/// Counts a statement for `/_metrics` and `pg_stat_statements`: its time
/// from arrival to answer, whether it failed, and the rows it returned or
/// changed.
fn counted(sql: &str, params: usize, took: Duration, failed: bool, rows: u64) {
    fenec_http::metrics::record(Transport::Pg, took, failed, || match params {
        0 => sql.to_string(),
        n => format!("{sql} ({n} parameters)"),
    });
    fenec_http::statements::rows(rows);
    SCOPE.with(|s| fenec_http::statements::record(s.borrow().as_deref(), sql, took, failed));
}

thread_local! {
    /// The tenant this connection's statements belong to, `None` over a
    /// single file.
    static SCOPE: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Whose statements `pg_stat_statements` shows this connection.
fn view_of(scope: &Option<String>) -> fenec_http::statements::View<'_> {
    match scope {
        Some(t) => fenec_http::statements::View::Tenant(t),
        None => fenec_http::statements::View::Node,
    }
}

/// Waits for the disk under `--sync always`, the lock already let go; a
/// write the disk refused has its answer replaced by the error.
fn durable_or_refused(
    db: &Arc<RwLock<Database>>,
    durability: Durability,
    answer: Option<usize>,
    out: &mut Writer,
) {
    if let Err(e) = durability() {
        fenec_http::log!("sync error: {e}");
        // The engine did not see this one fail: it is told, and refuses
        // every later write as after a failure of its own.
        write_lock(db).fail(&e);
        // An answer already written as if the write were durable is taken
        // back. On the paths that answered with the statement's own error,
        // that error stands, as it did when the sync ran under the lock.
        if let Some(mark) = answer {
            out.rewind(mark);
            out.error(sqlstate(&e), &e.to_string());
        }
    }
}

/// [`execute_into`]'s part under the lock. Returns what the answer has to
/// wait for to be durable, and where in `out` that answer begins when a
/// refusal has to replace it.
#[allow(clippy::too_many_arguments)]
fn run_locked(
    db: &Arc<RwLock<Database>>,
    tenant: &Held,
    frozen: bool,
    cfg: &Config,
    be: &Backend,
    tx: &mut TxState,
    lock: &mut Lock<'_>,
    sql: &str,
    parsed: Option<&[Statement]>,
    params: &[Value],
    out: &mut Writer,
    row_desc_sent: bool,
    formats: &[i16],
    pipeline: bool,
) -> Option<(Durability, Option<usize>)> {
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        out.empty_query();
        return None;
    }

    // A failed transaction takes nothing but its end, or a way back to a
    // savepoint before the failure.
    if tx.failed {
        return match compat::handle(trimmed, cfg, &|| false) {
            Some(compat::Shim::Tx(
                t @ (compat::Tx::Commit { .. }
                | compat::Tx::Rollback { .. }
                | compat::Tx::RollbackTo(_)),
            )) => tx.apply(t, lock, cfg, out),
            _ => {
                out.error(
                    "25P02",
                    "current transaction is aborted, commands ignored until end of \
                     transaction block",
                );
                None
            }
        };
    }

    // The standard queries PostgreSQL clients send at startup. A statement
    // parsed when it was prepared is none of them.
    let shim = match parsed {
        Some(_) => None,
        None => compat::handle(trimmed, cfg, &|| lock.read(db, |d| d.history().following)),
    };
    if let Some(shim) = shim {
        match shim {
            compat::Shim::Catalog => {
                let answer = catalog_answer(db, lock, cfg, trimmed, params);
                if !row_desc_sent {
                    out.row_description(&answer.columns, formats);
                }
                let at = out.mark();
                for row in &answer.rows {
                    if let Err(e) = text_row(out, &answer.columns, row, formats) {
                        out.rewind(at);
                        out.error("0A000", &e);
                        return None;
                    }
                }
                out.command_complete(&format!("SELECT {}", answer.rows.len()));
            }
            compat::Shim::Rows { columns, rows, tag } => {
                let cols: Vec<(String, i32)> =
                    columns.iter().map(|c| (c.clone(), OID_TEXT)).collect();
                if !row_desc_sent {
                    out.row_description(&cols, formats);
                }
                for row in &rows {
                    let cells: Vec<Option<String>> = row.iter().map(|c| Some(c.clone())).collect();
                    // Text is sent as its bytes in either format.
                    out.data_row(&cells);
                }
                out.command_complete(&format!("{tag} {}", rows.len()));
            }
            compat::Shim::Tag(tag) => out.command_complete(&tag),
            compat::Shim::Tx(t) => return tx.apply(t, lock, cfg, out),
            compat::Shim::Refuse { code, message } => out.error(code, &message),
        }
        return None;
    }

    let owned;
    let stmts: &[Statement] = match parsed {
        Some(s) => s,
        None => match read(trimmed) {
            Ok(s) => {
                owned = s;
                &owned
            }
            Err(e) => {
                out.error("42601", &e.to_string());
                return None;
            }
        },
    };
    if tx.open {
        tx.ran = true;
    }

    // A compact rewrites the file, which no block can put back, so it
    // cannot join one held open. Before a transaction's first write it runs
    // on its own, as outside one: it changes no document.
    let compact = stmts.iter().any(|s| !s.fits_block());
    if compact && lock.hold.is_some() {
        out.error(
            "25001",
            "compact cannot follow a write in a transaction or a pipeline: it rewrites the \
             file, which cannot be put back with them",
        );
        return None;
    }

    // `compact`, and a `create index` outside a transaction or a pipeline,
    // on their own are built beside the database: readers and writers go
    // on, and the write lock is taken only to put the result in place (see
    // `Database::maintain`). In a transaction a `create index` is one of
    // its writes, and is put back with them.
    if let [stmt @ (Statement::CreateIndex { .. } | Statement::Compact(_))] = stmts {
        let alone = !tx.open && !pipeline && lock.hold.is_none();
        if alone || matches!(stmt, Statement::Compact(_)) {
            if frozen {
                out.error("57P03", "the tenant is being moved; retry shortly");
                return None;
            }
            fenec_http::metrics::wrote();
            return maintain(db, cfg, stmt, out);
        }
    }

    // A shared lock suffices when everything is read-only: reads flow in parallel.
    let needs_write = stmts.iter().any(|s| !s.is_read_only());
    if needs_write && tx.open && tx.mode.read_only {
        out.error("25006", "cannot execute a write in a read-only transaction");
        return None;
    }
    // A frozen tenant is being exported for a move: the HTTP path answers
    // 503 with `Retry-After`, and this is that answer on the wire. Reads go
    // on -- the export is what they would read.
    if needs_write && frozen {
        out.error("57P03", "the tenant is being moved; retry shortly");
        return None;
    }
    if needs_write {
        fenec_http::metrics::wrote();
    }

    // A transaction takes the write lock at its first write -- at its first
    // statement if serializable -- and holds it to its end; a pipeline takes
    // it at a write with more of the pipeline to come, and holds it to its
    // Sync, as PostgreSQL runs a pipeline as one transaction.
    let takes = if tx.open {
        needs_write || tx.mode.serial
    } else {
        pipeline && needs_write
    };
    let mut first = None;
    if takes && !compact && lock.hold.is_none() {
        match lock.take(db, tenant, be, !tx.open) {
            Ok(turn) => first = Some(turn),
            Err((code, msg)) => {
                out.error(code, &msg);
                return None;
            }
        }
    }
    let held = lock.hold.is_some();
    let turn = match (first, &lock.hold) {
        (Some(t), _) => Some(Some(t)),
        (None, Some(h)) => Some(Turn::take(h.db, be)),
        (None, None) => None,
    };
    let mut guard = match turn {
        Some(Some(t)) => Guard::Turn(t),
        Some(None) if SHUTDOWN.load(Ordering::Relaxed) => {
            out.error("57P01", "the server is shutting down");
            return None;
        }
        Some(None) => {
            out.error("57014", "the query was cancelled");
            return None;
        }
        None => match acquire(db, needs_write, be) {
            Some(g) => g,
            // `acquire` returns `None` both on cancellation and on shutdown;
            // the two differ for the client: one can be retried, the other
            // means the connection is over.
            None if SHUTDOWN.load(Ordering::Relaxed) => {
                out.error("57P01", "the server is shutting down");
                return None;
            }
            None => {
                out.error("57014", "the query was cancelled");
                return None;
            }
        },
    };

    // A text of several statements with a write among them is one block,
    // as PostgreSQL runs a query of several as one transaction: its writes
    // land together, or -- an error, a cancel, the ceiling -- none of them.
    // A compact among them runs each on its own, as ever. Under a held lock
    // the statements join the block held open.
    let block = !held && needs_write && stmts.len() > 1 && !compact;
    if block {
        if let Err(e) = guard.begin() {
            out.error(sqlstate(&e), &e.to_string());
            return None;
        }
    }
    for (i, stmt) in stmts.iter().enumerate() {
        // A cancellation arriving mid-batch drops the rest. Outside a block
        // what ran before it stays applied, so under `always` it still goes
        // to disk; in one, it is put back.
        if be.take_cancel() {
            if block {
                guard.rollback();
            }
            let durability = guard.flush_if_needed(cfg.sync).ok().flatten();
            out.error("57014", "the query was cancelled");
            return durability.map(|d| (d, None));
        }
        // The memory ceiling is checked *before* the statement: the overshoot
        // is at most one statement, whose body is capped by `--max-message`.
        if let Some(msg) = fenec_http::over_ceiling(cfg.max_memory, guard.db(), stmt) {
            if block {
                guard.rollback();
            }
            let durability = guard.flush_if_needed(cfg.sync).ok().flatten();
            out.error("53200", &msg);
            return durability.map(|d| (d, None));
        }
        let last = i == stmts.len() - 1;
        let mut result = guard.run(stmt, params);
        if block && last && result.is_ok() {
            if let Err(e) = guard.commit() {
                result = Err(e);
            }
        }
        match result {
            Err(e) => {
                let code = sqlstate(&e);
                // In a block the statements before it are put back. Outside
                // one an error does not undo them: the `always` policy must
                // push those to disk as well. The statement's own error is
                // the one reported; a failed sync has already refused every
                // later write in the engine.
                if block {
                    guard.rollback();
                }
                let durability = guard.flush_if_needed(cfg.sync).ok().flatten();
                out.error(code, &e.to_string());
                return durability.map(|d| (d, None));
            }
            Ok(resp) => {
                if !last {
                    continue;
                }
                // Under `always` the answer waits for the disk: a write the
                // disk refused must not be reported as done.
                let durability = match guard.flush_if_needed(cfg.sync) {
                    Ok(d) => d,
                    Err(e) => {
                        out.error(sqlstate(&e), &e.to_string());
                        return None;
                    }
                };
                let answer = out.mark();
                match resp {
                    Response::Rows(rs) => {
                        // A PostgreSQL row is flat, so children are widened
                        // into the parent row the way a join would present
                        // them. Borrowed untouched when there are none.
                        let rs = rs.flatten();
                        let cols = match stmt {
                            Statement::Select(sel) => select_columns(guard.db(), sel),
                            _ => None,
                        }
                        .unwrap_or_else(|| {
                            rs.columns
                                .iter()
                                .map(|c| (c.clone(), OID_TEXT))
                                .collect::<Vec<_>>()
                        });
                        if !row_desc_sent {
                            out.row_description(&cols, formats);
                        }
                        let with_score = cols.last().map(|(n, _)| n == "_score").unwrap_or(false);
                        let binary = binary::any_binary(formats);
                        for row in &rs.rows {
                            if !binary {
                                let mut cells: Vec<Option<String>> =
                                    row.values.iter().map(to_pg_text).collect();
                                if with_score {
                                    cells.push(row.score.map(|s| format!("{s}")));
                                }
                                out.data_row(&cells);
                                continue;
                            }
                            let score = with_score.then_some(row.score);
                            match binary_row(&cols, &row.values, score, formats) {
                                Ok(cells) => out.data_row(&cells),
                                Err(e) => {
                                    out.rewind(answer);
                                    out.error("0A000", &e);
                                    return durability.map(|d| (d, None));
                                }
                            }
                        }
                        // PostgreSQL tags a plan `EXPLAIN`; psql prints the rows either way.
                        match stmt {
                            Statement::Explain(_) => out.command_complete("EXPLAIN"),
                            _ => out.command_complete(&format!("SELECT {}", rs.rows.len())),
                        }
                    }
                    Response::Affected(n) => {
                        let tag = match stmt {
                            Statement::Put { .. } => format!("INSERT 0 {n}"),
                            Statement::Update { .. } => format!("UPDATE {n}"),
                            Statement::Delete { .. } => format!("DELETE {n}"),
                            _ => format!("OK {n}"),
                        };
                        out.command_complete(&tag);
                    }
                    Response::Ok(_) => {
                        let tag = match stmt {
                            Statement::CreateCollection { .. } => "CREATE TABLE",
                            Statement::DropCollection { .. } => "DROP TABLE",
                            Statement::Compact(_) => "VACUUM",
                            _ => "OK",
                        };
                        out.command_complete(tag);
                    }
                    Response::Schemas(schemas) => {
                        if !row_desc_sent {
                            out.row_description(&schema_columns(), formats);
                        }
                        let mut n = 0;
                        for s in &schemas {
                            for f in &s.fields {
                                out.data_row(&[
                                    Some(s.name.clone()),
                                    Some(f.name.clone()),
                                    Some(f.ty.name()),
                                    Some(match &f.index {
                                        IndexKind::None => "-".to_string(),
                                        IndexKind::Hash => "hash".to_string(),
                                        IndexKind::Sorted => "sorted".to_string(),
                                        IndexKind::Vector(sp) => format!(
                                            "hnsw({}, m={}{})",
                                            sp.metric.name(),
                                            sp.m,
                                            sp.quant_arg()
                                        ),
                                        IndexKind::Text(sp) => {
                                            format!("text({})", sp.args())
                                        }
                                        IndexKind::Inverted => "inverted".to_string(),
                                    }),
                                ]);
                                n += 1;
                            }
                        }
                        out.command_complete(&format!("SELECT {n}"));
                    }
                }
                return durability.map(|d| (d, Some(answer)));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `parse_float_param` replaced `str::parse::<f64>()` here, so the only
    /// thing that matters is that it still answers exactly the same -- values
    /// and rejections both, bit for bit, NaN's sign bit included.
    #[track_caller]
    fn agrees(t: &str) {
        match (parse_float_param(t), t.parse::<f64>()) {
            (Some(a), Ok(b)) => assert_eq!(
                a.to_bits(),
                b.to_bits(),
                "{t:?}: {a} ({:#018x}) against {b} ({:#018x})",
                a.to_bits(),
                b.to_bits()
            ),
            (None, Err(_)) => {}
            (a, b) => panic!("{t:?}: parse_float_param gave {a:?}, str::parse gave {b:?}"),
        }
    }

    #[test]
    fn the_specials_are_spelt_the_same_way() {
        for t in [
            "inf",
            "INF",
            "Inf",
            "+inf",
            "-inf",
            "infinity",
            "Infinity",
            "INFINITY",
            "iNfInItY",
            "+infinity",
            "-infinity",
            "nan",
            "NaN",
            "NAN",
            "nAn",
            "+nan",
            "-nan",
        ] {
            agrees(t);
        }
        // The sign bit is the part an `is_nan()` check would miss.
        assert_eq!(
            parse_float_param("-nan").unwrap().to_bits(),
            0xfff8_0000_0000_0000
        );
        assert_eq!(
            parse_float_param("nan").unwrap().to_bits(),
            0x7ff8_0000_0000_0000
        );
    }

    #[test]
    fn near_misses_are_still_refused() {
        for t in [
            "infi",
            "in",
            "nanana",
            " inf",
            "inf ",
            "∞",
            "infinit",
            "infinityy",
            "na",
            "n",
            "-",
            "+",
            "",
            "inf1",
            "1inf",
            "--inf",
            "inf-",
        ] {
            agrees(t);
        }
    }

    #[test]
    fn ordinary_decimals_go_to_the_shared_parser() {
        for t in [
            "0",
            "-0",
            "1.5",
            "-0.04729",
            "1e10",
            "1e-300",
            "9007199254740993",
            "1.7976931348623157e308",
            "4.9406564584124654e-324",
            "2.2250738585072011e-308",
            "1e400",
            "-1e400",
            "+1.5",
            "0.1",
        ] {
            agrees(t);
        }
    }

    #[test]
    fn random_decimals_agree() {
        // The same xorshift the parser's own tests use, so a failure here is
        // reproducible the same way.
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = || {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            x = x.wrapping_mul(0x2545_f491_4f6c_dd1d);
            x
        };
        for _ in 0..20_000 {
            let f = f64::from_bits(next());
            if !f.is_finite() {
                continue;
            }
            agrees(&format!("{f}"));
            agrees(&format!("{f:e}"));
        }
    }
}
