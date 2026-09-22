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

use crate::catalog;
use crate::compat;
use crate::proto::*;
use crate::scram;
use fenec_core::json;
use fenec_core::prelude::*;
use fenec_core::query::projection_columns;
use fenec_core::value::DataType;
use fenec_http::metrics::Transport;
use fenec_ql::parse;
use std::collections::{HashMap, HashSet};
use std::io::{self, BufReader, BufWriter, Read, Write};
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
    /// and the next open does not rebuild it (100k x 128: 9.9 s -> 110 ms).
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

pub struct Server {
    db: Arc<RwLock<Database>>,
    cfg: Arc<Config>,
    backends: Backends,
    /// Number of live sessions. Incremented on accept, decremented via
    /// [`ConnGuard`] as the session thread ends.
    live: Arc<AtomicUsize>,
}

impl Server {
    pub fn new(db: Arc<RwLock<Database>>, cfg: Config) -> Server {
        Server {
            db,
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
        spawn_syncer(
            Arc::clone(&self.db),
            self.cfg.sync,
            self.cfg.checkpoint_on_exit,
        );
        fenec_http::log!(
            "fenec-pg {} listening on: postgres://localhost:{}/fenec  [{}, sync={}]",
            fenec_core::VERSION,
            listener.local_addr()?.port(),
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

            let db = Arc::clone(&self.db);
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
                    if let Err(e) = session(stream, db, cfg, backends) {
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
    let mut g = write_lock(db);
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
}

impl Guard<'_> {
    fn db(&self) -> &Database {
        match self {
            Guard::Read(g) => g,
            Guard::Write(g) => g,
        }
    }

    fn run(&mut self, stmt: &Statement, params: &[Value]) -> fenec_core::error::Result<Response> {
        match self {
            Guard::Read(g) => g.query(stmt, params),
            Guard::Write(g) => g.execute_with(stmt, params),
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
            _ => Ok(None),
        }
    }
}

/// A `create index` or `compact` run beside the database. Not cancellable:
/// the build holds no lock a `CancelRequest` could release.
fn maintain(
    db: &Arc<RwLock<Database>>,
    cfg: &Config,
    tx: &mut TxState,
    stmt: &Statement,
    out: &mut Writer,
) -> Option<(Durability, Option<usize>)> {
    if let Some(msg) = over_memory_cap(cfg, &read_lock(db), stmt) {
        out.error("53200", &msg);
        return None;
    }
    if let Err(e) = Database::maintain(db, stmt).expect("a maintenance statement") {
        out.error(sqlstate(&e), &e.to_string());
        return None;
    }
    if matches!(stmt, Statement::CreateIndex { .. }) {
        tx.note_write();
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
fn acquire<'a>(db: &'a RwLock<Database>, write: bool, be: &Backend) -> Option<Guard<'a>> {
    let mut tries = 0u32;
    loop {
        if write {
            match db.try_write() {
                Ok(g) => return Some(Guard::Write(g)),
                Err(TryLockError::Poisoned(g)) => return Some(Guard::Write(g.into_inner())),
                Err(TryLockError::WouldBlock) => {}
            }
        } else {
            match db.try_read() {
                Ok(g) => return Some(Guard::Read(g)),
                Err(TryLockError::Poisoned(g)) => return Some(Guard::Read(g.into_inner())),
                Err(TryLockError::WouldBlock) => {}
            }
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

// ------------------------------------------------------------------ session

#[derive(Default, Clone)]
struct Prepared {
    sql: String,
}

#[derive(Default, Clone)]
struct Portal {
    sql: String,
    stmt_name: String,
    params: Vec<Value>,
}

fn session(
    stream: TcpStream,
    db: Arc<RwLock<Database>>,
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

    loop {
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

        match m.tag {
            // ------------------------------------------------ simple query
            b'Q' => {
                let mut pos = 0;
                let sql = take_cstr(&m.body, &mut pos);
                be.busy.store(true, Ordering::SeqCst);
                be.canceled.store(false, Ordering::SeqCst);
                execute_into(&db, &cfg, &be, &mut tx, &sql, &[], &mut out, false);
                be.busy.store(false, Ordering::SeqCst);
                be.canceled.store(false, Ordering::SeqCst);
                out.ready(tx.status());
                out.flush_to(&mut w)?;
            }

            // ------------------------------------------------ extended
            b'P' => {
                let mut pos = 0;
                let name = take_cstr(&m.body, &mut pos);
                let sql = take_cstr(&m.body, &mut pos);
                described_stmts.remove(&name);
                prepared.insert(name, Prepared { sql });
                out.parse_complete();
            }
            b'B' => {
                let mut pos = 0;
                let portal = take_cstr(&m.body, &mut pos);
                let stmt = take_cstr(&m.body, &mut pos);
                let sql = prepared
                    .get(&stmt)
                    .map(|p| p.sql.clone())
                    .unwrap_or_default();

                // parameter format codes
                let nfmt = be_i16(&m.body, &mut pos);
                let mut fmts = Vec::new();
                for _ in 0..nfmt {
                    fmts.push(be_i16(&m.body, &mut pos));
                }
                // parameter values
                let nparams = be_i16(&m.body, &mut pos);
                let mut values = Vec::new();
                for i in 0..nparams {
                    let len = be_i32(&m.body, &mut pos);
                    if len < 0 {
                        values.push(Value::Null);
                        continue;
                    }
                    let raw = &m.body[pos..pos + len as usize];
                    pos += len as usize;
                    let binary = fmts.get(i as usize).or(fmts.first()).copied().unwrap_or(0) == 1;
                    values.push(decode_param(raw, binary));
                }
                described_portals.remove(&portal);
                portals.insert(
                    portal,
                    Portal {
                        sql,
                        stmt_name: stmt,
                        params: values,
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
                let sql = if kind == b'S' {
                    prepared.get(&name).map(|p| p.sql.clone())
                } else {
                    portals.get(&name).map(|p| p.sql.clone())
                };
                let sql = sql.unwrap_or_default();
                be.busy.store(true, Ordering::SeqCst);
                let shape = describe(&db, &cfg, &sql, &be);
                be.busy.store(false, Ordering::SeqCst);
                be.canceled.store(false, Ordering::SeqCst);
                let shape = match shape {
                    Some(s) => s,
                    None => {
                        out.error("57014", "the query was cancelled");
                        out.ready(tx.status());
                        out.flush_to(&mut w)?;
                        continue;
                    }
                };
                if kind == b'S' {
                    out.parameter_description(&shape.params);
                }
                match &shape.columns {
                    Some(cols) => {
                        out.row_description(cols);
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
                let p = portals.get(&portal).cloned().unwrap_or_default();
                // If RowDescription was already sent with Describe we do not
                // repeat it (the protocol says so); when Describe was skipped
                // it is sent anyway, so the client is not left without column
                // names.
                let already =
                    described_portals.contains(&portal) || described_stmts.contains(&p.stmt_name);
                be.busy.store(true, Ordering::SeqCst);
                be.canceled.store(false, Ordering::SeqCst);
                execute_into(
                    &db, &cfg, &be, &mut tx, &p.sql, &p.params, &mut out, already,
                );
                be.busy.store(false, Ordering::SeqCst);
                be.canceled.store(false, Ordering::SeqCst);
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
                out.ready(tx.status());
                out.flush_to(&mut w)?;
            }
            b'H' => {
                out.flush_to(&mut w)?;
            }
            b'X' => return Ok(()),
            other => {
                out.error("0A000", &format!("unsupported message `{}`", other as char));
                out.ready(tx.status());
                out.flush_to(&mut w)?;
            }
        }
    }
}

/// Cleans the record up when the session ends (on a panic too).
/// Whether the data ceiling is exceeded. Only statements that *grow* the
/// data are stopped: `del` and `compact` are deliberately left out, because
/// they are the way out of a database that has hit the ceiling. Reads are
/// unaffected anyway.
fn over_memory_cap(cfg: &Config, db: &Database, stmt: &Statement) -> Option<String> {
    let grows = matches!(
        stmt,
        Statement::Put { .. } | Statement::Update { .. } | Statement::CreateIndex { .. }
    );
    if cfg.max_memory == 0 || !grows {
        return None;
    }
    let used = db.memory_bytes();
    if used < cfg.max_memory {
        return None;
    }
    Some(format!(
        "data ceiling exceeded: {} / {}. Writes have stopped; run `del` + \
         `compact` to make room, or raise --max-memory",
        human(used),
        human(cfg.max_memory)
    ))
}

/// Write KiB rather than saying `0 MiB` for small values.
fn human(bytes: usize) -> String {
    if bytes >= 1 << 20 {
        format!("{} MiB", bytes >> 20)
    } else {
        format!("{} KiB", bytes >> 10)
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
fn decode_param(raw: &[u8], binary: bool) -> Value {
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

fn pg_oid(ty: &DataType) -> i32 {
    match ty {
        DataType::Bool => OID_BOOL,
        DataType::Int => OID_INT8,
        DataType::Float => OID_FLOAT8,
        DataType::Bytes => OID_BYTEA,
        DataType::Timestamp => OID_TIMESTAMPTZ,
        // vectors and lists travel as text (pgvector notation)
        _ => OID_TEXT,
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
        Value::Float(f) => format!("{f}"),
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
                s.push_str(&format!("{x}"));
            }
            s.push(']');
            s
        }
        Value::List(items) => {
            // PostgreSQL array notation: {a,b,c}
            let mut s = String::from("{");
            for (i, x) in items.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                match x {
                    Value::Text(t) => s.push_str(&format!("\"{}\"", t.replace('"', "\\\""))),
                    other => s.push_str(&to_pg_text(other).unwrap_or_else(|| "NULL".into())),
                }
            }
            s.push('}');
            s
        }
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
    if sel.near.is_some() {
        cols.push(("_score".to_string(), OID_FLOAT8));
    }
    Some(cols)
}

// ------------------------------------------------------------------ Describe

/// The `Describe` response: expected parameters and (when there are rows) the row format.
struct Shape {
    params: Vec<i32>,
    /// `None` -> `NoData`
    columns: Option<Vec<(String, i32)>>,
}

/// A catalog query run over the schemas as they stand: its columns with
/// their types, and its rows. One the catalog cannot read answers empty.
fn catalog_answer(
    db: &RwLock<Database>,
    cfg: &Config,
    sql: &str,
    params: &[Value],
) -> catalog::Answer {
    // The schemas are copied under the read lock and the query runs without
    // it: a catalog join is cheap, but a writer need not wait for one.
    let snap = catalog::Snapshot::of(&read_lock(db), "fenec", &cfg.server_version);
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
fn describe(db: &RwLock<Database>, cfg: &Config, sql: &str, be: &Backend) -> Option<Shape> {
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        return Some(Shape {
            params: Vec::new(),
            columns: None,
        });
    }
    // Compatibility-layer queries are pure and fixed; the shape is read from there.
    if let Some(shim) = compat::handle(trimmed, cfg, &|| read_lock(db).history().following) {
        return Some(match shim {
            compat::Shim::Rows { columns, .. } => Shape {
                params: Vec::new(),
                columns: Some(columns.into_iter().map(|c| (c, OID_TEXT)).collect()),
            },
            // A catalog query's columns do not depend on its parameters: it
            // is run with every one null to learn them.
            compat::Shim::Catalog => {
                let n = catalog::params(trimmed).unwrap_or(0);
                let answer = catalog_answer(db, cfg, trimmed, &vec![Value::Null; n]);
                Shape {
                    params: vec![OID_UNSPECIFIED; n],
                    columns: Some(answer.columns),
                }
            }
            // A refusal is reported by Execute, the way a syntax error is.
            compat::Shim::Tag(_) | compat::Shim::Tx(_) | compat::Shim::Refuse { .. } => Shape {
                params: Vec::new(),
                columns: None,
            },
        });
    }
    let stmts = match parse(trimmed) {
        Ok(s) => s,
        // A syntax error is reported during Execute; we do not branch
        // Describe off with a second error message.
        Err(_) => {
            return Some(Shape {
                params: Vec::new(),
                columns: None,
            })
        }
    };
    let nparams = stmts.iter().map(|s| s.max_param()).max().unwrap_or(0);
    // Types are not resolved: seeing `unspecified`, the client sends the
    // value as text and `decode_param` infers it.
    let params = vec![OID_UNSPECIFIED; nparams];

    let columns = match stmts.last() {
        Some(Statement::Select(sel)) => {
            // Reading the schema needs a shared lock; a cancellation arriving
            // while waiting behind a long write has to be seen here too.
            let guard = acquire(db, false, be)?;
            // When the collection does not exist yet we cannot know the
            // shape; rather than erroring we say NoData and let Execute speak.
            select_columns(guard.db(), sel)
        }
        Some(Statement::ListCollections) | Some(Statement::Describe(_)) => Some(schema_columns()),
        Some(Statement::Explain(_)) => {
            Some(vec![(fenec_core::query::PLAN_COLUMN.to_string(), OID_TEXT)])
        }
        _ => None,
    };
    Some(Shape { params, columns })
}

// ---------------------------------------------------------------- execution

/// What a session did since `BEGIN`. There are no transactions -- every
/// statement is applied as it runs -- but drivers still bracket their work
/// with `BEGIN` and `COMMIT`/`ROLLBACK`, and a `ROLLBACK` that answers
/// "done" after a write is a wrong answer believed right: the caller thinks
/// the writes were undone. So the session counts the writes it lets through
/// and `ROLLBACK` succeeds only when there is nothing it would have had to
/// undo, which keeps it harmless for the pools that send it on every
/// check-in.
#[derive(Default)]
struct TxState {
    open: bool,
    writes: usize,
}

impl TxState {
    /// The ReadyForQuery status. It has to say `T` inside a block: libpq-based
    /// drivers such as psycopg 3 read their transaction state from it, and
    /// with a permanent `I` they never send the `ROLLBACK` at all.
    fn status(&self) -> u8 {
        if self.open {
            b'T'
        } else {
            b'I'
        }
    }

    /// Called for a statement that left something behind, which the caller
    /// reads off the change counter rather than the statement's result: a
    /// `put` can fail after writing part of its documents, and one that
    /// failed validation wrote nothing a `ROLLBACK` would have had to undo.
    fn note_write(&mut self) {
        if self.open {
            self.writes += 1;
        }
    }

    fn apply(&mut self, tx: compat::Tx, out: &mut Writer) {
        match tx {
            // A second `BEGIN` keeps the block, and the count, it is in.
            compat::Tx::Begin => {
                if !self.open {
                    *self = TxState {
                        open: true,
                        writes: 0,
                    };
                }
                out.command_complete("BEGIN");
            }
            compat::Tx::Commit => {
                *self = TxState::default();
                out.command_complete("COMMIT");
            }
            compat::Tx::Rollback => {
                let n = self.writes;
                *self = TxState::default();
                if n == 0 {
                    out.command_complete("ROLLBACK");
                } else {
                    let (s, are) = if n == 1 { ("", "is") } else { ("s", "are") };
                    out.error(
                        "0A000",
                        &format!(
                            "ROLLBACK undid nothing: fenecdb has no transactions, and the \
                             {n} write statement{s} since BEGIN {are} already applied"
                        ),
                    );
                }
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
#[allow(clippy::too_many_arguments)]
fn execute_into(
    db: &Arc<RwLock<Database>>,
    cfg: &Config,
    be: &Backend,
    tx: &mut TxState,
    sql: &str,
    params: &[Value],
    out: &mut Writer,
    row_desc_sent: bool,
) {
    let started = Instant::now();
    let errors = out.errors();
    let wait = run_locked(db, cfg, be, tx, sql, params, out, row_desc_sent);
    if let Some((durability, answer)) = wait {
        durable_or_refused(db, durability, answer, out);
    }
    fenec_http::metrics::record(
        Transport::Pg,
        started.elapsed(),
        out.errors() > errors,
        || match params.len() {
            0 => sql.to_string(),
            n => format!("{sql} ({n} parameters)"),
        },
    );
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
    cfg: &Config,
    be: &Backend,
    tx: &mut TxState,
    sql: &str,
    params: &[Value],
    out: &mut Writer,
    row_desc_sent: bool,
) -> Option<(Durability, Option<usize>)> {
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        out.empty_query();
        return None;
    }

    // The standard queries PostgreSQL clients send at startup
    if let Some(shim) = compat::handle(trimmed, cfg, &|| read_lock(db).history().following) {
        match shim {
            compat::Shim::Catalog => {
                let answer = catalog_answer(db, cfg, trimmed, params);
                if !row_desc_sent {
                    out.row_description(&answer.columns);
                }
                for row in &answer.rows {
                    out.data_row(row);
                }
                out.command_complete(&format!("SELECT {}", answer.rows.len()));
            }
            compat::Shim::Rows { columns, rows, tag } => {
                if !row_desc_sent {
                    let cols: Vec<(String, i32)> =
                        columns.iter().map(|c| (c.clone(), OID_TEXT)).collect();
                    out.row_description(&cols);
                }
                for row in &rows {
                    let cells: Vec<Option<String>> = row.iter().map(|c| Some(c.clone())).collect();
                    out.data_row(&cells);
                }
                out.command_complete(&format!("{tag} {}", rows.len()));
            }
            compat::Shim::Tag(tag) => out.command_complete(&tag),
            compat::Shim::Tx(t) => tx.apply(t, out),
            compat::Shim::Refuse { code, message } => out.error(code, &message),
        }
        return None;
    }

    let stmts = match parse(trimmed) {
        Ok(s) => s,
        Err(e) => {
            out.error("42601", &e.to_string());
            return None;
        }
    };

    // `create index` and `compact` on their own are built beside the
    // database: readers and writers go on, and the write lock is taken only
    // to put the result in place (see `Database::maintain`).
    if let [stmt @ (Statement::CreateIndex { .. } | Statement::Compact(_))] = stmts.as_slice() {
        fenec_http::metrics::wrote();
        return maintain(db, cfg, tx, stmt, out);
    }

    // A shared lock suffices when everything is read-only: reads flow in parallel.
    let needs_write = stmts.iter().any(|s| !s.is_read_only());
    if needs_write {
        fenec_http::metrics::wrote();
    }
    let mut guard = match acquire(db, needs_write, be) {
        Some(g) => g,
        // `acquire` returns `None` both on cancellation and on shutdown; the
        // two differ for the client: one can be retried, the other means the
        // connection is over.
        None if SHUTDOWN.load(Ordering::Relaxed) => {
            out.error("57P01", "the server is shutting down");
            return None;
        }
        None => {
            out.error("57014", "the query was cancelled");
            return None;
        }
    };

    for (i, stmt) in stmts.iter().enumerate() {
        // A cancellation arriving mid-batch drops the rest. What ran before
        // it stays applied, so under `always` it still goes to disk.
        if be.take_cancel() {
            let durability = guard.flush_if_needed(cfg.sync).ok().flatten();
            out.error("57014", "the query was cancelled");
            return durability.map(|d| (d, None));
        }
        // The memory ceiling is checked *before* the statement: the overshoot
        // is at most one statement, whose body is capped by `--max-message`.
        if let Some(msg) = over_memory_cap(cfg, guard.db(), stmt) {
            let durability = guard.flush_if_needed(cfg.sync).ok().flatten();
            out.error("53200", &msg);
            return durability.map(|d| (d, None));
        }
        let last = i == stmts.len() - 1;
        // The change counter moves for every document and schema change, and
        // for nothing else.
        let before = guard.db().change_seq();
        let result = guard.run(stmt, params);
        if guard.db().change_seq() != before {
            tx.note_write();
        }
        match result {
            Err(e) => {
                let code = sqlstate(&e);
                // An error does not undo the statements written before it
                // (there are no transactions): the `always` policy must push
                // those to disk as well. The statement's own error is the one
                // reported; a failed sync has already refused every later
                // write in the engine.
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
                            out.row_description(&cols);
                        }
                        let with_score = cols.last().map(|(n, _)| n == "_score").unwrap_or(false);
                        for row in &rs.rows {
                            let mut cells: Vec<Option<String>> =
                                row.values.iter().map(to_pg_text).collect();
                            if with_score {
                                cells.push(row.score.map(|s| format!("{s}")));
                            }
                            out.data_row(&cells);
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
                            out.row_description(&schema_columns());
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
                                        IndexKind::Vector(sp) => {
                                            format!("hnsw({}, m={})", sp.metric.name(), sp.m)
                                        }
                                        IndexKind::Text(sp) => {
                                            format!("text(k1={}, b={})", sp.k1(), sp.b())
                                        }
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
