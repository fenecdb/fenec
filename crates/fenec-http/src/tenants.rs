//! Many databases behind one listener: one `.fenec` file per tenant.
//!
//! **Why a file per tenant and not a tenant field.** A tenant is picked by
//! the request (`/t/<name>/...`), never by the query text, so nothing
//! rewrites a query to add `tenant = ...` -- and two tenants sharing a
//! collection would then read each other's rows. A file each keeps them
//! apart by construction, and settles four things a shared file would have
//! to solve: ids are counted per file, the `/changes` sequence is one number
//! per file, BM25 statistics are the tenant's own, and a `lookup` never
//! crosses a file.
//!
//! **Opened on first use, not at start.** Open cost is proportional to the
//! file (the whole image is read and replayed), so a node with thousands of
//! tenants would spend its start-up on tenants nobody asks for. A tenant is
//! opened by the first request that names it and closed again by
//! [`Tenants::close_idle`] once nothing holds it.
//!
//! **One instance per file, always.** Two `Database` values over one file
//! corrupt it, exactly as two processes would. A tenant is opened and
//! closed under its own slot lock, and closed only when the slot holds the
//! last reference -- so a stream or a request still using it keeps it open,
//! and a request arriving mid-close waits for the close to finish before it
//! opens the file again.

use crate::lease::Lease;
use crate::node_stream::NodeStreams;
use crate::quota::{self, Held as QuotaHeld, Kind, Quota, Set};
use crate::replication::{fresh_id, Follower, Replication, Said};
use crate::sse::Hub;
use fenec_core::history::History;
use fenec_core::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

/// Ceiling on a tenant name. It becomes a file name, so it is also kept to
/// a character set no file system treats specially.
pub const MAX_NAME: usize = 64;

/// How long `delete` waits for the requests and streams on a tenant to let
/// go before it gives up with 409.
const RELEASE_WAIT: Duration = Duration::from_secs(5);

type Setup = Box<dyn Fn(&mut Database) -> Result<()> + Send + Sync>;

/// How a node replicates its tenants: every tenant file is opened through a
/// feed and serves `/t/<tenant>/_replication`, the way a single file serves
/// `/_replication`. With `upstream` this node is a standby: each tenant it
/// opens follows the tenant of the same name on that node.
///
/// A tenant can also follow a node of its own, whichever holds its primary
/// ([`Tenants::follow`] with `from`), kept in `<tenant>.follows` beside its
/// file: the router spreading one node's tenants over the others' replicas
/// rather than onto a standby that waits idle, and takes them all when the
/// node goes. The token is this one, the cluster's.
pub struct Replicated {
    pub token: String,
    /// Bytes of writes kept per tenant for a replica that falls behind.
    pub buffer: usize,
    pub upstream: Option<String>,
    /// Whether a write is fsynced before it is answered: a replica is sent
    /// only what an fsync covered.
    pub sync_on_write: bool,
}

/// A registry failure, already shaped as an HTTP status and message.
#[derive(Debug)]
pub struct Refused(pub u16, pub String);

impl Refused {
    /// The answer it is. A 503 -- the node going down -- is told to come
    /// back in a second, as a frozen tenant's write is: through the router
    /// the tenant may be served by another node by then.
    pub fn response(self) -> crate::http::Response {
        match self.0 {
            503 => crate::http::Response::retry_after(503, &self.1, 1),
            status => crate::http::Response::error(status, &self.1),
        }
    }
}

pub struct Tenant {
    name: String,
    pub db: Arc<RwLock<Database>>,
    pub hub: Arc<Hub>,
    /// The feed replicas of this tenant read, and the follower it runs on a
    /// replica node. `None` unless the node replicates (`Replicated`).
    pub repl: Option<Arc<Replication>>,
    /// Held shared by every request for the length of its handling, and
    /// exclusively by `freeze`. A write that passed the frozen check but has
    /// not taken the database lock yet would otherwise land *after* the
    /// export that was meant to be final.
    gate: RwLock<()>,
    frozen: AtomicBool,
    /// Milliseconds since the registry's epoch.
    last_used: AtomicU64,
    /// The node's lease, on a node that takes one.
    lease: Option<Arc<Lease>>,
    /// Its quota and what its requests hold of it now ([`crate::quota`]).
    pub quota: Arc<QuotaHeld>,
    /// Whether its file is known to hold a prefix of its tenant's history:
    /// a primary's always, a promoted copy's from its promotion; a
    /// follower's once, since this open, the primary it follows has said
    /// hello -- its image adopted, or its place in that history found. Not
    /// kept past an open: a read replica made a moment ago is an empty file
    /// of a history of its own, and answering a marked read from it told a
    /// client the tenant had no collections; a file reopened, or pointed at
    /// another node, may be one whose history the primary's does not
    /// continue -- a copy left on a node gone for good while its tenant was
    /// deleted and made again -- until the primary has said so.
    ready: Arc<AtomicBool>,
    /// Which open of its file this is, and of the follower started with
    /// it: a refusal deletes the copy only while that open still holds the
    /// slot ([`Tenants::delete_refused`]).
    opened: u64,
}

/// The opens of tenants' files in this process, numbered.
static OPENS: AtomicU64 = AtomicU64::new(1);

/// What a router knows of a tenant it promotes a copy of (`POST
/// /_admin/tenants/<t>/promote`'s body), which the copy is held to
/// ([`Tenants::promote_as`]).
#[derive(Clone, Debug, Default)]
pub struct Expected {
    /// The lineage the tenant is of: a copy that took its primary's image
    /// is of it, one that never did is of its own -- a new file's, or none
    /// on a standby -- and so is a file put in its place by hand.
    pub lineage: Option<u32>,
    /// The history the tenant's primary forked at its last failover, which
    /// a copy that has heard from that primary holds among its own: an old
    /// primary's copy made to follow it and never reached holds the history
    /// it had, which the lineage alone does not tell from the tenant's. A
    /// copy that heard from the node it follows since it was told to
    /// follow it passes without it: a primary's file restored in place to a
    /// moment before that failover holds the history it had then, and so
    /// does every copy that took it, though each holds every write its
    /// primary has.
    pub history: Option<u64>,
    /// Promoted whatever it holds: an operator's word.
    pub force: bool,
}

/// The line `.follows` takes once the copy heard from the node it was
/// told to follow ([`mark_heard`]).
const HEARD: &str = "heard";

/// Writes a tenant's `.follows` whole or not at all: beside, synced, then
/// renamed, and the rename made durable -- until its directory is synced a
/// power loss can bring back the file before, the node it followed then, or
/// a copy marked heard that has heard nothing since its follow.
fn write_follows(dir: &Path, name: &str, text: &str) -> std::io::Result<()> {
    let tmp = dir.join(format!("{name}.follows.writing"));
    let path = dir.join(format!("{name}.follows"));
    std::fs::File::create(&tmp)
        .and_then(|mut f| {
            use std::io::Write;
            f.write_all(text.as_bytes())?;
            f.sync_all()
        })
        .and_then(|_| std::fs::rename(&tmp, &path))
        .and_then(|_| dir_synced(&path))
}

/// A tenant's `<file>.lock` (`fenec_abi::lock`), taken before its file is
/// opened or put in its place, as `fenec-server --file`, the shell and an
/// app take a file's: refused while another process -- a shell, an app,
/// another node over the directory -- has the file open, naming it. Held
/// in this process, it is a database of the tenant not let go of yet,
/// which a second over the file would corrupt as surely: waited for up to
/// [`RELEASE_WAIT`], as a close, a delete or a follow lets the tenant go
/// with what ran beside it still ending -- a follower's thread, a linking
/// slice, the graph keeper's append -- then refused.
fn hold_tenant(name: &str, path: &Path) -> std::result::Result<fenec_abi::lock::FileLock, Refused> {
    fenec_abi::lock::hold_within(path, RELEASE_WAIT).map_err(|e| match e {
        fenec_abi::lock::Refused::Held => {
            let holder = fenec_abi::lock::holder(path);
            let held = match &holder {
                Some(h) if h.pid == Some(std::process::id()) => {
                    "open in this process already, a database of it not let go of yet".into()
                }
                Some(h) if h.is_known() => format!("open in another process ({h})"),
                _ => "open in another process".into(),
            };
            Refused(
                500,
                format!("tenant `{name}`'s file is {held}: two writers over one file corrupt it"),
            )
        }
        fenec_abi::lock::Refused::Io(e) => Refused(
            500,
            format!("could not take tenant `{name}`'s lock file: {e}"),
        ),
    })
}

/// [`fenec_core::fs::sync_dir`] as an I/O error.
fn dir_synced(path: &Path) -> std::io::Result<()> {
    fenec_core::fs::sync_dir(path).map_err(|e| std::io::Error::other(e.to_string()))
}

/// Notes in a tenant's `.follows` that its copy has heard from the node it
/// was told to follow -- its image taken, or its place in that node's
/// history found -- once since the follow wrote it ([`Tenants::follow`]):
/// kept across a restart, where the copy's `ready` is not, and asked as a
/// promotion is ([`refusal`]). Called on the follower's thread, which a
/// follow halts before it writes the file again, so no hello of the follow
/// before lands after it. A copy with no `.follows` was never told to
/// follow -- a standby's made with its node -- and notes nothing.
fn mark_heard(dir: &Path, name: &str) {
    let Ok(text) = std::fs::read_to_string(dir.join(format!("{name}.follows"))) else {
        return;
    };
    if text.lines().any(|l| l.trim() == HEARD) {
        return;
    }
    let marked = format!("{}\n{HEARD}", text.trim_end_matches('\n'));
    if let Err(e) = write_follows(dir, name, &marked) {
        crate::log!("tenant `{name}` heard from its primary, but could not note it: {e}");
    }
}

/// Why a copy is not to be promoted as `want` would have it, if it is not.
/// Asked of a following copy alone: a file taking writes is refused as no
/// replica further on. `heard` is whether it heard from the node it follows
/// since it was last told to follow it ([`Tenants::heard_since_told`]).
fn refusal(
    name: &str,
    g: &Database,
    ready: bool,
    heard: Option<bool>,
    want: &Expected,
) -> Option<String> {
    let h = g.history();
    if !h.following {
        return None;
    }
    let text = crate::replication::lineage_text;
    let ours = crate::replication::lineage_of(h);
    if let Some(l) = want.lineage.filter(|l| ours != Some(*l)) {
        return Some(format!(
            "tenant `{name}` here is of lineage {}, not {}, the tenant's: it has not taken \
             its primary's image -- a copy made to follow a primary it never reached, or \
             another file put in its place -- and promoted it would be an empty primary, or \
             another file's",
            ours.map_or("none".into(), text),
            text(l)
        ));
    }
    if let Some(id) = want
        .history
        .filter(|id| heard != Some(true) && h.lineage.iter().all(|e| e.0 != *id))
    {
        let told = match heard {
            Some(_) => "has heard nothing from the node it follows since it was told to follow it",
            None => "keeps no word of hearing from the node it follows",
        };
        return Some(format!(
            "tenant `{name}` here lacks the history the tenant's primary forked at its last \
             failover ({id:016x}) and {told}: it may be an older primary's file, without the \
             writes taken since, and promoted it would take them back"
        ));
    }
    // Nothing said of the tenant -- a router from before lineages were
    // kept, a tenant made before it kept them, an operator's call naming
    // none -- the one copy known to hold nothing of it: no write taken, and
    // no hello from its primary since it opened nor since it was told to
    // follow it. A standby's copy of a tenant that took no write either,
    // its node restarted since, is refused with it, for the operator to
    // force.
    let empty = g.change_seq() == 0 && !ready && heard != Some(true);
    if want.lineage.is_none() && want.history.is_none() && empty {
        return Some(format!(
            "tenant `{name}` here has taken no write and has not heard from its primary since \
             it opened, and nothing was said of the lineage it is to be of: it may hold \
             nothing of the tenant"
        ));
    }
    None
}

impl Tenant {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether a read its client marked may be answered from this copy
    /// ([`Self::ready`]'s field): one not ready is answered 503, which a
    /// router takes to the tenant's home. A copy taking writes is always
    /// ready, whatever made it a primary: asked only where the field says
    /// no, which a following copy says until its primary's hello.
    pub fn ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
            || !self
                .db
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .history()
                .following
    }

    pub fn is_frozen(&self) -> bool {
        self.frozen.load(Ordering::SeqCst)
    }

    /// Whether something beside the tenant holds its database: a compact
    /// or a warm-up run beside the queries, a linking slice, the graph
    /// keeper's append. Its follower's hold is the tenant's own. Closed
    /// meanwhile, the database outlived the tenant, holding the file's
    /// lock, and the next request's open waited [`RELEASE_WAIT`] for it
    /// and answered 500 -- each request, for as long as a compact ran.
    fn db_in_use(&self) -> bool {
        if late_holders::listed(&self.name) {
            return false;
        }
        let follower = self.repl.as_ref().and_then(|r| r.follower()).is_some();
        Arc::strong_count(&self.db) > 1 + follower as usize
    }

    /// Whether this node's lease lets it write the tenant now, and why not:
    /// on a node that takes no lease, always. The database refuses the
    /// same writes itself (`Database::set_fence`); this is for answering
    /// before one is tried.
    pub fn writable(&self) -> std::result::Result<(), String> {
        match &self.lease {
            Some(l) => l.allows(&self.name),
            None => Ok(()),
        }
    }

    /// Holds the tenant against `freeze` while a request is being handled.
    pub fn enter(&self) -> RwLockReadGuard<'_, ()> {
        self.gate.read().unwrap_or_else(|e| e.into_inner())
    }

    fn read(&self) -> RwLockReadGuard<'_, Database> {
        self.db.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, Database> {
        self.db.write().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for Tenant {
    /// The last reference is going: whatever is buffered reaches the disk.
    /// `FileSink` would flush on its own drop, but without an fsync.
    fn drop(&mut self) {
        let mut g = self.write();
        if g.is_dirty() {
            if let Err(e) = g.sync() {
                crate::log!("sync error ({}): {e}", self.name);
            }
        }
    }
}

pub struct Tenants {
    dir: PathBuf,
    /// Only the tenants that are open, or being opened or closed right now.
    /// Held for a lookup and nothing longer: opening a file (seconds for a
    /// large one with no graph in it) happens under the tenant's own slot
    /// lock, so a slow open stalls requests for that tenant and no other.
    slots: Mutex<HashMap<String, Arc<Slot>>>,
    shutting_down: AtomicBool,
    epoch: Instant,
    setup: Option<Setup>,
    repl: Option<Replicated>,
    change_capacity: usize,
    max_memory: usize,
    checkpoint: bool,
    /// Whether tenant files are mapped (`fs::open`) or read into memory
    /// (`fenec-server --no-mmap`).
    mapped: bool,
    /// The router's lease, on a node that takes one (`--lease`).
    lease: Option<Arc<Lease>>,
    /// Whether every tenant's database checks what a scoped token writes
    /// ([`Tenants::check_scoped_writes`]).
    checked: AtomicBool,
    /// The derived indexes built as a tenant opens (`--warm`).
    warm: Option<crate::warm::Warm>,
    /// Every tenant's quota but what its `<tenant>.quota` sets
    /// (`--tenant-max-bytes` and the rest).
    quota: Quota,
    /// The node's streams of every tenant, on a node that replicates its
    /// tenants ([`crate::node_stream`]).
    streams: Option<Arc<NodeStreams>>,
    /// Where each tenant that closed in this process stood: its change and
    /// its history, by which the node's stream judges a closed tenant
    /// without opening it. Only this process writes a tenant's file, and
    /// only while it is open, so it stands there until it opens again.
    closed: Mutex<HashMap<String, (u64, History)>>,
    /// When each tenant deleted in this process went, by the node's clock:
    /// the time the node's stream tells an archive it is gone, as a write's
    /// is the time the node appended it -- told when the stream came to
    /// it, a moment between the two, the stream busy or the archiver away,
    /// restored a tenant the node no longer held. Kept only on a node that
    /// replicates its tenants (`streams`), where an archiver can ask: up to
    /// [`DEPARTED_KEPT`] names, a few megabytes, for nothing elsewhere.
    departed: Mutex<Departed>,
    /// The registry itself, once it is shared ([`Tenants::shared`]): what
    /// a copy refusing another tenant's primary deletes itself through.
    me: std::sync::OnceLock<std::sync::Weak<Tenants>>,
}

/// The copies that refused a primary of another lineage than the one they
/// were told -- a tenant made again under their name -- and deleted
/// themselves (`fenec_copies_refused_total`).
static COPIES_REFUSED: AtomicU64 = AtomicU64::new(0);

/// How many copies refused another tenant's primary since the process
/// started.
pub fn copies_refused() -> u64 {
    COPIES_REFUSED.load(Ordering::Relaxed)
}

/// The tenants that left, the last [`DEPARTED_KEPT`] of them: what an
/// archiver told late needs, not every name a node ever held.
#[derive(Default)]
struct Departed {
    at: HashMap<String, u64>,
    order: std::collections::VecDeque<(String, u64)>,
}

const DEPARTED_KEPT: usize = 65_536;

impl Departed {
    fn note(&mut self, name: &str, at: u64) {
        self.at.insert(name.into(), at);
        self.order.push_back((name.into(), at));
        while self.order.len() > DEPARTED_KEPT {
            if let Some((old, when)) = self.order.pop_front() {
                if self.at.get(&old) == Some(&when) {
                    self.at.remove(&old);
                }
            }
        }
    }
}

/// A tenant as the node's stream finds it, without opening it.
pub(crate) enum Seen {
    /// No file.
    Missing,
    /// Being opened or closed now.
    Busy,
    Open(Arc<Tenant>),
    /// Closed: where it stood when it closed, where it closed in this
    /// process.
    Closed(Option<(u64, History)>),
}

/// One tenant's open/closed state. Opening and closing a tenant both happen
/// under this lock, which is what makes "one instance per file" hold: a
/// request arriving mid-close waits and then opens the file again, rather
/// than opening it next to the instance still being closed.
struct Slot {
    held: Mutex<Held>,
}

enum Held {
    Closed,
    Open(Arc<Tenant>),
}

impl Tenants {
    /// A registry over `dir`, created when missing. Nothing is opened yet.
    pub fn new(dir: impl Into<PathBuf>) -> std::io::Result<Tenants> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Tenants {
            dir,
            slots: Mutex::new(HashMap::new()),
            shutting_down: AtomicBool::new(false),
            epoch: Instant::now(),
            setup: None,
            repl: None,
            change_capacity: fenec_core::changes::DEFAULT_CAPACITY,
            max_memory: 0,
            checkpoint: true,
            mapped: true,
            lease: None,
            checked: AtomicBool::new(false),
            warm: None,
            quota: Quota::default(),
            streams: None,
            closed: Mutex::new(HashMap::new()),
            departed: Mutex::new(Departed::default()),
            me: std::sync::OnceLock::new(),
        })
    }

    /// Notes the `Arc` the registry is served from, which a copy refusing
    /// another tenant's primary deletes itself through
    /// ([`crate::Server::with_tenants`]).
    pub fn shared(self: &Arc<Self>) {
        let _ = self.me.set(Arc::downgrade(self));
    }

    /// When `name` was last deleted in this process, ms since the epoch by
    /// the node's clock.
    pub(crate) fn departed(&self, name: &str) -> Option<u64> {
        self.departed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .at
            .get(name)
            .copied()
    }

    /// Holds every tenant to `q`, but what a tenant's own quota sets
    /// ([`Self::set_quota`]): the node's defaults.
    pub fn with_quota(mut self, q: Quota) -> Tenants {
        self.quota = q;
        self
    }

    /// The node's default quota.
    pub fn default_quota(&self) -> Quota {
        self.quota
    }

    /// Whether tenant files are mapped, as `fs::open` maps a file, or read
    /// into memory: `fenec-server --no-mmap`, which a network file system wants
    /// -- a read error there kills a process reading a mapping -- and which
    /// has `--max-memory` count the data.
    pub fn with_mmap(mut self, on: bool) -> Tenants {
        self.mapped = on;
        self
    }

    /// Builds each tenant's derived indexes as it opens, beside its first
    /// requests (`fenec-server --warm`, [`crate::warm`]).
    pub fn with_warm(mut self, warm: Option<crate::warm::Warm>) -> Tenants {
        self.warm = warm;
        self
    }

    /// Runs on every database as it is opened -- `fenec-server` installs its
    /// plugin here, so a tenant sees the same functions a single file does.
    pub fn with_setup(
        mut self,
        f: impl Fn(&mut Database) -> Result<()> + Send + Sync + 'static,
    ) -> Tenants {
        self.setup = Some(Box::new(f));
        self
    }

    /// Opens every tenant through a feed, so replicas can be fed from it,
    /// and follows another node's tenants when `upstream` is set.
    pub fn with_replication(mut self, repl: Replicated) -> Tenants {
        self.repl = Some(repl);
        self.streams = Some(NodeStreams::new());
        self
    }

    /// The node's streams of every tenant, when it replicates its tenants.
    pub fn node_streams(&self) -> Option<&Arc<NodeStreams>> {
        self.streams.as_ref()
    }

    /// Tells the node's streams that `name` has news.
    fn touch(&self, name: &str) {
        if let Some(s) = &self.streams {
            s.touch(name);
        }
    }

    /// The tenants open now.
    pub(crate) fn open_now(&self) -> Vec<Arc<Tenant>> {
        self.open_tenants()
    }

    /// Whether `name` has a file on this node.
    pub(crate) fn on_disk(&self, name: &str) -> bool {
        self.path(name).exists()
    }

    /// A tenant as it is now, opened by nobody for it.
    pub(crate) fn seen(&self, name: &str) -> Seen {
        let slot = self.registry().get(name).cloned();
        if let Some(slot) = slot {
            match slot.held.try_lock() {
                Ok(held) => {
                    if let Held::Open(t) = &*held {
                        return Seen::Open(Arc::clone(t));
                    }
                }
                Err(_) => return Seen::Busy,
            }
        }
        if !self.on_disk(name) {
            return Seen::Missing;
        }
        let closed = self.closed.lock().unwrap_or_else(|e| e.into_inner());
        Seen::Closed(closed.get(name).cloned())
    }

    /// Where a closed tenant stands -- its change and history -- read from
    /// its file as a tool that only looks reads one (`fs::open_read_only`):
    /// under its slot, so nothing opens it meanwhile, and kept as a close
    /// keeps it. `None` when it is open or has no file. Opened to be
    /// written, a tenant's file is fsynced first (a replica must be sent
    /// only what is on disk), `F_FULLFSYNC` on macOS: a stream's first look
    /// at 40 closed tenants took 251 ms that way, 6.3 ms a tenant, where a
    /// first pass reading 1 001, one of 64 MB, took 0.29-0.33 s whole
    /// (`make node-archive-bench`). It sends nothing, so the fsync is the
    /// open's to make, once an image is to go out. A sync log not yet
    /// applied (Linux, after the machine went down) would leave it reading
    /// short of the file: it reads nothing then, and the tenant is opened.
    pub(crate) fn peek(&self, name: &str) -> std::result::Result<Option<(u64, History)>, Refused> {
        check_name(name)?;
        self.with_slot(name, |held| {
            if !matches!(held, Held::Closed) || !self.on_disk(name) {
                return Ok(None);
            }
            // A sync log beside it (Linux) holds durable writes the file
            // lacks until it is opened to be written, which applies the
            // log: read short of them, a client standing at the short file's
            // change was sent none of them. Where it stands is not known
            // then, and the tenant is opened.
            let log = fenec_core::fs::beside(&self.path(name), "sync");
            if std::fs::metadata(&log).is_ok_and(|m| m.len() > 0) {
                return Ok(None);
            }
            let db = fenec_core::fs::open_read_only(self.path(name))
                .map_err(|e| Refused(500, format!("could not read tenant `{name}`: {e}")))?;
            let place = (db.change_seq(), db.history().clone());
            self.closed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(name.to_string(), place.clone());
            Ok(Some(place))
        })
    }

    /// Runs `f` on the tenant, opened for it when it is not open and closed
    /// again after -- with no checkpoint, as nothing was written -- where
    /// nothing else took it meanwhile: the node's stream looking at a
    /// tenant it cannot tell the place of, after the node started, holds
    /// one open at a time rather than every one until it idles out.
    pub(crate) fn look<R>(
        &self,
        name: &str,
        f: impl FnOnce(&Arc<Tenant>) -> R,
    ) -> std::result::Result<R, Refused> {
        let (t, opened) = self.get_noting(name)?;
        let r = f(&t);
        drop(t);
        if opened {
            self.with_slot(name, |held| self.close_with(held, false));
        }
        Ok(r)
    }

    /// Writes a tenant only while the router's lease names it and has not
    /// lapsed ([`Lease`]): starting with none, the node takes no write until
    /// its router grants one.
    pub fn with_lease(mut self) -> Tenants {
        self.lease = Some(Arc::new(Lease::new()));
        self
    }

    /// The lease, on a node that takes one.
    pub fn lease(&self) -> Option<&Arc<Lease>> {
        self.lease.as_ref()
    }

    /// The token a tenant's `/_replication` asks for, when the node
    /// replicates its tenants.
    /// The token a tenant's stream and the node's are asked for: none where
    /// it is empty, which a request with no `Authorization` would match.
    pub fn replication_token(&self) -> Option<&str> {
        Some(self.repl.as_ref()?.token.as_str()).filter(|t| !t.is_empty())
    }

    pub fn with_change_capacity(mut self, n: usize) -> Tenants {
        self.change_capacity = n;
        self
    }

    /// Ceiling on the summed footprint of the *open* tenants (0 = off).
    /// Opening one more over it first closes idle tenants, oldest first.
    pub fn with_max_memory(mut self, bytes: usize) -> Tenants {
        self.max_memory = bytes;
        self
    }

    /// Whether closing a tenant (idle, delete, shutdown) writes a checkpoint
    /// when it has a vector index -- the same trade as `--no-checkpoint`.
    pub fn with_checkpoint(mut self, on: bool) -> Tenants {
        self.checkpoint = on;
        self
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn registry(&self) -> MutexGuard<'_, HashMap<String, Arc<Slot>>> {
        self.slots.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.fenec"))
    }

    /// Where a tenant that follows a node of its own keeps that node's URL.
    fn follows_path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.follows"))
    }

    /// Where a tenant keeps the quota set for it over the node's.
    fn quota_path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.quota"))
    }

    /// What is set for a tenant over the node's quota: nothing where no
    /// file says. A file that does not read keeps the tenant from opening
    /// rather than open it held to the node's quota alone, which may be
    /// none.
    fn quota_set(&self, name: &str) -> std::result::Result<Set, Refused> {
        match std::fs::read_to_string(self.quota_path(name)) {
            Ok(text) => Set::parse(&text).map_err(|e| {
                Refused(
                    500,
                    format!("tenant `{name}`'s quota ({name}.quota) does not read: {e}"),
                )
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Set::default()),
            Err(e) => Err(Refused(
                500,
                format!("tenant `{name}`'s quota could not be read: {e}"),
            )),
        }
    }

    /// The node a tenant's file follows when it is a replica's: its own,
    /// recorded by [`Self::follow`], or this node's standby upstream.
    fn upstream_of(&self, name: &str) -> Option<String> {
        let own = std::fs::read_to_string(self.follows_path(name)).ok();
        let first = own.and_then(|text| text.lines().next().map(|l| l.trim().to_string()));
        match first.filter(|u| !u.is_empty()) {
            Some(url) => Some(url),
            None => self.repl.as_ref()?.upstream.clone(),
        }
    }

    /// The lineage a tenant was told its primary is of, which `.follows`
    /// keeps on a line of its own after the address (`lineage <8 hex>`):
    /// a restart keeps refusing another tenant of the name.
    fn lineage_followed(&self, name: &str) -> Option<u32> {
        let text = std::fs::read_to_string(self.follows_path(name)).ok()?;
        text.lines()
            .find_map(|l| l.trim().strip_prefix("lineage "))
            .and_then(|l| crate::replication::parse_lineage(l.trim()))
    }

    /// Whether a tenant's copy has heard from the node it follows since it
    /// was last told to follow it (`.follows`' `heard` line, [`mark_heard`]):
    /// `None` where it was never told, a standby's copy made with its node
    /// or a file from before the mark.
    fn heard_since_told(&self, name: &str) -> Option<bool> {
        let text = std::fs::read_to_string(self.follows_path(name)).ok()?;
        Some(text.lines().any(|l| l.trim() == HEARD))
    }

    /// Forgets the node a tenant followed, durably: a promoted file follows
    /// nobody, and kept -- or brought back by a power loss before its
    /// directory is synced -- the record would have the next open make it
    /// follow its old primary again.
    fn forget_following(&self, name: &str) -> std::io::Result<()> {
        let path = self.follows_path(name);
        match std::fs::remove_file(&path) {
            Ok(()) => dir_synced(&path),
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    /// Records the node a tenant is told to follow -- none, on a standby,
    /// for the node's own upstream, an empty line -- and the lineage it is
    /// told that node's tenant is of, as not heard from yet: whole or not at
    /// all, written beside and renamed, as a tenant's file is installed.
    fn record_following(
        &self,
        name: &str,
        url: Option<&str>,
        lineage: Option<u32>,
    ) -> std::io::Result<()> {
        let mut text = url.unwrap_or("").to_string();
        if let Some(l) = lineage {
            text.push_str("\nlineage ");
            text.push_str(&crate::replication::lineage_text(l));
        }
        write_follows(&self.dir, name, &text)
    }

    /// Opens every tenant that follows a node of its own, and on a standby
    /// every tenant: a follower that is not running is a replica falling
    /// behind, and nothing else opens a replica's tenant. What did not
    /// open, and why.
    pub fn resume_following(&self) -> Vec<(String, Refused)> {
        let standby = self.repl.as_ref().is_some_and(|r| r.upstream.is_some());
        let mut failed = Vec::new();
        for name in self.names() {
            if !standby && !self.follows_path(&name).exists() {
                continue;
            }
            if let Err(e) = self.get(&name) {
                failed.push((name, e));
            }
        }
        failed
    }

    fn now(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// Runs `f` on the tenant's slot, locked. The slot is re-checked against
    /// the registry once locked: one that was dropped in the meantime (the
    /// tenant closed and a new slot took its place) is not the one to act
    /// on, and acting on it could open a second instance of the file. A
    /// slot left closed is dropped from the registry on the way out, so the
    /// map holds open tenants only -- a request naming tenants that do not
    /// exist leaves nothing behind.
    ///
    /// Lock order is slot, then registry; nothing takes a slot lock while
    /// holding the registry.
    fn with_slot<R>(&self, name: &str, f: impl FnOnce(&mut Held) -> R) -> R {
        loop {
            let slot = Arc::clone(self.registry().entry(name.to_string()).or_insert_with(|| {
                Arc::new(Slot {
                    held: Mutex::new(Held::Closed),
                })
            }));
            let mut held = slot.held.lock().unwrap_or_else(|e| e.into_inner());
            let current = self
                .registry()
                .get(name)
                .is_some_and(|s| Arc::ptr_eq(s, &slot));
            if !current {
                continue;
            }
            let out = f(&mut held);
            if matches!(*held, Held::Closed) {
                let mut reg = self.registry();
                if reg.get(name).is_some_and(|s| Arc::ptr_eq(s, &slot)) {
                    reg.remove(name);
                }
            }
            return out;
        }
    }

    /// A snapshot of the slots, taken without holding the registry after.
    /// Has every tenant's database test what a scoped token writes against
    /// its rules (`access::Check`), as [`crate::Server::new`] has a single
    /// database: those open now and each opened after. A node serving JSON
    /// Web Tokens without it took a scoped write outside the token's
    /// filter, `WITH CHECK` being a hook no tenant had installed.
    pub fn check_scoped_writes(&self) {
        self.checked.store(true, Ordering::SeqCst);
        for slot in self.all_slots() {
            let held = slot.held.lock().unwrap_or_else(|e| e.into_inner());
            if let Held::Open(t) = &*held {
                // A second install finds it there and is refused: harmless.
                let _ = crate::held::write(&t.db).install_plugin(&crate::access::CheckPlugin);
            }
        }
    }

    fn all_slots(&self) -> Vec<Arc<Slot>> {
        self.registry().values().cloned().collect()
    }

    fn refuse_if_shutting_down(&self) -> std::result::Result<(), Refused> {
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(Refused(503, "the node is shutting down".into()));
        }
        Ok(())
    }

    /// The tenant, opened if it is on disk and not open yet. 404 when there
    /// is no such file: a tenant comes into being through `create`, never
    /// by a request naming it, or a typo would make an empty database.
    pub fn get(&self, name: &str) -> std::result::Result<Arc<Tenant>, Refused> {
        self.get_noting(name).map(|(t, _)| t)
    }

    /// [`Self::get`], and whether this call opened it.
    fn get_noting(&self, name: &str) -> std::result::Result<(Arc<Tenant>, bool), Refused> {
        check_name(name)?;
        let t = self.with_slot(name, |held| {
            if let Held::Open(t) = held {
                t.last_used.store(self.now(), Ordering::Relaxed);
                return Ok((Arc::clone(t), false));
            }
            self.refuse_if_shutting_down()?;
            let path = self.path(name);
            if !path.exists() {
                return Err(Refused(404, format!("no tenant `{name}` on this node")));
            }
            let t = self.open_file(name, &path)?;
            *held = Held::Open(Arc::clone(&t));
            Ok((t, true))
        });
        let (t, opened) = t?;
        if opened {
            self.make_room(name);
        }
        Ok((t, opened))
    }

    /// Takes this tenant's writes on a replica node: its history forks
    /// here and the follower stops. The failover is one tenant at a time,
    /// because a node's tenants are separate databases -- there is nothing
    /// to make atomic between them.
    pub fn promote(&self, name: &str) -> std::result::Result<(u64, u64), Refused> {
        self.promote_as(name, &Expected::default())
    }

    /// [`Tenants::promote`], the copy held to what the router knows of its
    /// tenant ([`Expected`]): one that is not the tenant as its primary has
    /// it is refused, 409, unless `force`.
    pub fn promote_as(
        &self,
        name: &str,
        want: &Expected,
    ) -> std::result::Result<(u64, u64), Refused> {
        let promoted = self.promote_held(name, want);
        // The fork's history reached the feed as its record was appended;
        // the node's stream sends it on before the writes after it.
        self.touch(name);
        promoted
    }

    fn promote_held(
        &self,
        name: &str,
        want: &Expected,
    ) -> std::result::Result<(u64, u64), Refused> {
        let t = self.get(name)?;
        // A copy that is not its tenant as the primary has it is refused:
        // promoted, a copy that never took its primary's image was an empty
        // primary, and the tenant's rows, whole on its old primary's node,
        // were wiped from there once that node came back and its copy was
        // made to follow this one; an old primary's copy made to follow the
        // tenant's new primary, promoted before it heard from it, took back
        // every write since the failover before. Refused, the tenant stays
        // where its rows are until that node returns.
        let heard = self.heard_since_told(name);
        let refused = refusal(name, &t.read(), t.ready.load(Ordering::SeqCst), heard, want);
        match (refused, want.force) {
            (Some(why), false) => {
                return Err(Refused(
                    409,
                    format!("{why}. {{\"force\": true}} promotes it all the same"),
                ))
            }
            (refused, true) => {
                // The operator's deliberate word, in the audit log whatever
                // it overrode: what the tenant had since is theirs to lose.
                let overrode = refused.as_deref().unwrap_or("");
                crate::log!("tenant `{name}` promoted by force: {overrode}");
                crate::audit::event(
                    "promote_forced",
                    &[
                        ("tenant", crate::audit::Field::Text(name)),
                        ("overrode", crate::audit::Field::Text(overrode)),
                    ],
                );
            }
            (None, false) => {}
        }
        // Promoted, it follows nobody: the record would have the next open
        // make it follow its old primary again.
        // And it is ready: a promoted copy that never heard from its primary
        // since it opened -- a standby's, which a node pair's restart left
        // waiting for a hello no primary now sends -- answered every marked
        // read 503 for as long as it stayed open.
        let forget = |done| {
            t.ready.store(true, Ordering::SeqCst);
            self.forget_following(name)
                .map_err(|e| Refused(500, format!("tenant `{name}` promoted, but {e}")))?;
            Ok(done)
        };
        if let Some(follower) = t.repl.as_ref().and_then(|r| r.follower()) {
            let done = follower
                .promote(fresh_id())
                .map_err(|e| Refused(500, e.to_string()))?;
            return forget(done);
        }
        // A replica's file with nothing following for it -- a node started
        // without --replica-of, after a failover left it unpromoted -- is
        // promoted where it stands, as `fenec-server --promote` promotes a file.
        // It could not be before: it opened refusing writes, and nothing on
        // the running node could make it take them.
        let mut g = crate::held::write(&t.db);
        if !g.history().following {
            return Err(Refused(409, format!("tenant `{name}` is not a replica")));
        }
        let id = crate::replication::fork_id(g.history(), fresh_id());
        g.fork(id)
            .and_then(|_| g.sync())
            .map_err(|e| Refused(500, e.to_string()))?;
        forget((g.change_seq(), id))
    }

    /// Has a tenant follow `from`, the node its primary is on -- or, with
    /// none, the node this one is the standby of: a primary's file made a
    /// replica's, or a replica's made to follow the node its tenant moved
    /// to. What the router asks of the node it places a replica on, and of
    /// a node rejoining after a failover, for each tenant it held: the
    /// writes that node took that were never sent are gone, the trade a
    /// failover makes.
    pub fn follow(&self, name: &str, from: Option<&str>) -> std::result::Result<(), Refused> {
        self.follow_lineage(name, from, None)
    }

    /// [`Tenants::follow`], the tenant told the lineage its primary is of
    /// (a router's read replica): kept in `.follows`, and a primary of
    /// another taken nothing from ([`Follower::expect_lineage`]).
    pub fn follow_lineage(
        &self,
        name: &str,
        from: Option<&str>,
        lineage: Option<u32>,
    ) -> std::result::Result<(), Refused> {
        check_name(name)?;
        let Some(repl) = &self.repl else {
            return Err(Refused(
                409,
                "this node replicates nothing (--replication-token)".into(),
            ));
        };
        match from {
            Some(url) => {
                crate::replication::Upstream::check_node(url).map_err(|_| {
                    Refused(
                        400,
                        format!("`{url}` is not a node's address: http://host:port"),
                    )
                })?;
            }
            None if repl.upstream.is_none() => {
                return Err(Refused(
                    409,
                    "this node is nobody's standby (--replica-of): name the node to follow, \
                     {\"from\": \"http://host:port\"}"
                        .into(),
                ));
            }
            _ => {}
        }
        let wanted = from.map(str::to_string).or_else(|| repl.upstream.clone());
        let followed = self.with_slot(name, |held| {
            self.refuse_if_shutting_down()?;
            let path = self.path(name);
            if let Held::Open(t) = &*held {
                if t.read().history().following && self.upstream_of(name) == wanted {
                    // Following that node already: its stream is begun again
                    // and the copy is not ready until the node says hello.
                    // The tenant there may be another than the one it heard
                    // -- deleted and made again while this copy was cut off
                    // -- and a router placing a read replica over a file it
                    // found on the node asks for no more than this follow.
                    // Halted before the record is written again, not heard
                    // from: a hello of the stream before, noted after it,
                    // would mark heard a copy that heard nothing since.
                    let follower = t.repl.as_ref().and_then(|r| r.follower());
                    if let Some(f) = follower {
                        f.halt();
                    }
                    let recorded = self.record_following(name, from, lineage).map_err(|e| {
                        Refused(
                            500,
                            format!("could not record the node tenant `{name}` follows: {e}"),
                        )
                    });
                    if let Some(f) = follower {
                        if recorded.is_ok() {
                            f.expect_lineage(lineage);
                            t.ready.store(false, Ordering::SeqCst);
                        }
                        f.start(format!("fenec-replica-{name}")).map_err(|e| {
                            Refused(500, format!("tenant `{name}` could not follow again: {e}"))
                        })?;
                    }
                    return recorded;
                }
                // Closed first: one instance over the file, always. What it
                // served ends -- a primary's file made a replica's feeds a
                // replica no longer -- and its follower, which holds the
                // database rather than the tenant, is halted, or it would go
                // on writing the file under the next instance.
                if t.is_frozen() {
                    return Err(Refused(409, format!("tenant `{name}` is frozen")));
                }
                release(t, name)?;
                *held = Held::Closed;
            }
            if !path.exists() {
                return Err(Refused(404, format!("no tenant `{name}` on this node")));
            }
            self.record_following(name, from, lineage).map_err(|e| {
                Refused(
                    500,
                    format!("could not record the node tenant `{name}` follows: {e}"),
                )
            })?;
            let t = self.open_as(name, &path, true)?;
            *held = Held::Open(t);
            Ok(())
        });
        self.touch(name);
        followed
    }

    /// Creates an empty tenant. 409 when it exists.
    pub fn create(&self, name: &str) -> std::result::Result<Arc<Tenant>, Refused> {
        check_name(name)?;
        let t = self.with_slot(name, |held| {
            let path = self.path(name);
            if matches!(held, Held::Open(_)) || path.exists() {
                return Err(Refused(409, format!("tenant `{name}` already exists")));
            }
            self.refuse_if_shutting_down()?;
            let t = self.open_file(name, &path)?;
            *held = Held::Open(Arc::clone(&t));
            Ok(t)
        })?;
        self.make_room(name);
        self.touch(name);
        Ok(t)
    }

    fn open_file(&self, name: &str, path: &Path) -> std::result::Result<Arc<Tenant>, Refused> {
        self.open_as(name, path, false)
    }

    /// Opens a tenant's file; `demote` makes a primary's file follow.
    fn open_as(
        &self,
        name: &str,
        path: &Path,
        demote: bool,
    ) -> std::result::Result<Arc<Tenant>, Refused> {
        let failed = |e: fenec_core::error::Error| {
            Refused(500, format!("could not open tenant `{name}`: {e}"))
        };
        // `<file>.lock` before the open, held by the database's sink and let
        // go with the database, whoever drops it last. Taken by nothing, a
        // shell over `acme.fenec` beside the node appended its write where
        // the node appended its next, and the file opened no more.
        let lock = hold_tenant(name, path)?;
        let (mut db, feed) = match &self.repl {
            None => {
                let wrap = fenec_abi::lock::holding(lock);
                let db = fenec_core::fs::open_serving(path, self.mapped, Box::new(wrap));
                (db.map_err(failed)?, None)
            }
            Some(r) => {
                let file = path.to_string_lossy().into_owned();
                let (db, feed) =
                    crate::replication::open_serving_holding(&file, r.buffer, self.mapped, lock)
                        .map_err(failed)?;
                (db, Some(feed))
            }
        };
        // A tenant's role is its file's, not the node's. A replica's file
        // goes on following; a primary's stays one, a tenant a failover
        // promoted here included; and a file new to a replica node follows,
        // being the standby copy the router made. Every file on a node with
        // --replica-of was made to follow once, so a promotion was undone at
        // the next open -- an idle close, a restart with the same flags --
        // and the old primary's image then wiped the writes taken since. A
        // rejoining node's tenants follow when told to (`follow`).
        let history = db.history().clone();
        let upstream = self.upstream_of(name);
        let following = match (&upstream, history.following) {
            (Some(_), true) => true,
            (Some(_), false) if demote || history.lineage.is_empty() => {
                db.follow(history.lineage.clone()).map_err(failed)?;
                true
            }
            (Some(_), false) => false,
            (None, false) if self.repl.is_some() && history.lineage.is_empty() => {
                db.fork(fresh_id())
                    .and_then(|_| db.sync())
                    .map_err(failed)?;
                false
            }
            (None, true) => {
                crate::log!(
                    "tenant `{name}` is a replica's file and this node follows no other: \
                     it takes no write until POST /_admin/tenants/{name}/promote"
                );
                false
            }
            (None, false) => false,
        };
        if let Some(setup) = &self.setup {
            setup(&mut db).map_err(|e| Refused(500, e.to_string()))?;
        }
        // Its bytes and rows are asked of each block as it lands, where
        // they are bounded: a tenant with neither has nothing asked.
        let held = QuotaHeld::new(self.quota_set(name)?.over(&self.quota));
        db.set_limit(quota::limit(&held));
        if self.checked.load(Ordering::SeqCst) {
            let _ = db.install_plugin(&crate::access::CheckPlugin);
        }
        // Every write the database lands asks the lease first, whichever
        // way it came in: HTTP, a maintenance, a follower.
        if let Some(lease) = &self.lease {
            let (lease, name) = (Arc::clone(lease), name.to_string());
            db.set_fence(Some(Arc::new(move || lease.allows(&name))));
        }
        let hub = Hub::new();
        db.set_watcher(Arc::clone(&hub) as Arc<dyn Watcher>);
        db.set_change_capacity(self.change_capacity);
        // Before anything can write it -- its follower starts below: the
        // node's streams that stand where the file does send from its
        // feed, and its writes wait for them.
        if let (Some(streams), Some(feed)) = (&self.streams, &feed) {
            streams.attach(name, feed, db.change_seq(), db.history());
        }
        let db = Arc::new(RwLock::new(db));
        crate::link::beside(&format!("tenant `{name}`"), &db);
        let bytes = Arc::clone(&held);
        crate::link::keep_held(
            &format!("tenant `{name}`"),
            &db,
            Some(Arc::new(move || bytes.get(Kind::Bytes))),
        );
        crate::sweep::watch(&format!("tenant `{name}`"), &db);
        if let Some(w) = &self.warm {
            crate::warm::start(&format!("tenant `{name}`"), &db, w);
        }
        // The follower applies the primary node's writes for this tenant, and
        // holds the database -- not the tenant -- while it runs; `close`
        // therefore keeps a tenant with a running follower open.
        // A primary's file holds its own history; a follower's a prefix of
        // its primary's once that primary has said hello since this open.
        let ready = Arc::new(AtomicBool::new(!following));
        let opened = OPENS.fetch_add(1, Ordering::Relaxed);
        let mut follower = None;
        if let (Some(r), Some(url), true) = (&self.repl, &upstream, following) {
            let started = Follower::new(
                &format!("{}/t/{name}", url.trim_end_matches('/')),
                r.token.clone(),
                Arc::clone(&db),
                feed.clone(),
                r.sync_on_write,
            )
            .map_err(|e| Refused(500, format!("tenant `{name}` cannot follow {url}: {e}")))
            .and_then(|f| {
                // Ready at the primary's hello, for as long as this open
                // lasts -- and no more where the primary is refused as
                // another tenant of the name, whose image it never takes.
                let seen = Arc::clone(&ready);
                let (me, who) = (self.me.get().cloned(), name.to_string());
                let dir = self.dir.clone();
                f.on_hello(Box::new(move |said| match said {
                    // Noted before it reads as ready, so a copy found ready
                    // has its word on disk: noted after, a restart in between
                    // forgot a hello its reads had been answered after.
                    Said::Taken => {
                        mark_heard(&dir, &who);
                        seen.store(true, Ordering::SeqCst);
                    }
                    Said::Refused { expected } => {
                        seen.store(false, Ordering::SeqCst);
                        refused_copy(me.clone(), &who, opened, expected);
                    }
                }));
                f.expect_lineage(self.lineage_followed(name));
                f.start(format!("fenec-replica-{name}")).map_err(|e| {
                    Refused(
                        500,
                        format!("tenant `{name}` could not start following: {e}"),
                    )
                })?;
                Ok(f)
            });
            match started {
                Ok(f) => follower = Some(f),
                Err(e) => {
                    if let (Some(streams), Some(feed)) = (&self.streams, &feed) {
                        streams.detach(name, feed);
                    }
                    return Err(e);
                }
            }
        }
        let repl = self
            .repl
            .as_ref()
            .map(|r| Replication::new(Some(r.token.clone()), feed.clone(), follower));
        Ok(Arc::new(Tenant {
            name: name.to_string(),
            db,
            hub,
            repl,
            gate: RwLock::new(()),
            frozen: AtomicBool::new(false),
            last_used: AtomicU64::new(self.now()),
            lease: self.lease.clone(),
            quota: held,
            ready,
            opened,
        }))
    }

    /// Sets what a tenant's quota is over the node's, kept beside its file
    /// -- written beside and renamed, whole or not at all -- and held to at
    /// once where the tenant is open: its next request, and its next block
    /// as it lands. An empty set leaves the tenant at the node's quota.
    /// What it is held to now.
    pub fn set_quota(&self, name: &str, set: Set) -> std::result::Result<Quota, Refused> {
        check_name(name)?;
        let quota = set.over(&self.quota);
        self.with_slot(name, |held| {
            if !self.path(name).exists() {
                return Err(Refused(404, format!("no tenant `{name}` on this node")));
            }
            let path = self.quota_path(name);
            let written = match set.is_empty() {
                true => match std::fs::remove_file(&path) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                    _ => Ok(()),
                },
                false => {
                    let tmp = self.dir.join(format!("{name}.quota.writing"));
                    std::fs::File::create(&tmp)
                        .and_then(|mut f| {
                            use std::io::Write;
                            f.write_all(set.json().as_bytes())?;
                            f.sync_all()
                        })
                        .and_then(|_| std::fs::rename(&tmp, &path))
                }
            };
            written.map_err(|e| {
                Refused(500, format!("could not write tenant `{name}`'s quota: {e}"))
            })?;
            if let Held::Open(t) = &*held {
                t.quota.set(quota);
                crate::held::write(&t.db).set_limit(quota::limit(&t.quota));
            }
            Ok(quota)
        })
    }

    /// A tenant's quota and what it uses of it, as JSON: what is set for
    /// it, what it is held to, and its file's bytes, rows, streams and
    /// requests in flight now. A tenant that is not open is not opened for
    /// it: its file's size is the disk's, and its rows unknown (`null`).
    pub fn quota_report(&self, name: &str) -> std::result::Result<String, Refused> {
        check_name(name)?;
        if !self.path(name).exists() {
            return Err(Refused(404, format!("no tenant `{name}` on this node")));
        }
        let set = self.quota_set(name)?;
        let open = self.open_tenants().into_iter().find(|t| t.name == name);
        let (quota, usage) = match &open {
            Some(t) => {
                let (bytes, rows) = {
                    let g = t.read();
                    (g.garbage().file, Some(g.documents()))
                };
                // Counted where the quota is set; without one, the
                // subscriptions its hub counts in any case.
                let (streams, requests, _) = t.quota.holding();
                let streams = match t.quota.get(Kind::Streams) {
                    0 => t.hub.live() as u64,
                    _ => streams,
                };
                let held = t.hub.waits().stats().held as u64;
                (
                    t.quota.quota(),
                    usage_json(bytes, rows, Some(streams), requests, Some(held)),
                )
            }
            None => {
                let bytes = std::fs::metadata(self.path(name)).map_or(0, |m| m.len());
                (
                    set.over(&self.quota),
                    usage_json(bytes, None, None, 0, None),
                )
            }
        };
        let mut out = String::from("{\"tenant\":");
        fenec_core::json::escape_into(&mut out, name);
        out.push_str(&format!(
            ",\"open\":{},\"set\":{},\"quota\":{},\"usage\":{usage}}}",
            open.is_some(),
            set.json(),
            quota.json()
        ));
        Ok(out)
    }

    /// Every tenant's [`Self::quota_report`], a JSON array: what a router
    /// joins into its list of tenants for the studio. No tenant is opened.
    pub fn quota_reports(&self) -> String {
        let reports: Vec<String> = self
            .names()
            .iter()
            .filter_map(|n| self.quota_report(n).ok())
            .collect();
        format!("[{}]", reports.join(","))
    }

    /// Closes the slot's tenant if nothing but the slot holds it or its
    /// database and it is not frozen: checkpoint when there is a graph to
    /// keep, then the last `Arc` goes and `Drop` does the final sync -- all
    /// under the slot lock.
    fn close(&self, held: &mut Held) -> bool {
        self.close_with(held, self.checkpoint)
    }

    /// [`Self::close`], with a checkpoint where `checkpoint` and the tenant
    /// has a graph to keep.
    fn close_with(&self, held: &mut Held, checkpoint: bool) -> bool {
        let Held::Open(t) = &*held else {
            return false;
        };
        if Arc::strong_count(t) > 1 || t.is_frozen() || t.db_in_use() {
            return false;
        }
        // Its follower holds the database rather than the tenant: closed, the
        // tenant would leave it writing the file, and the next open -- a
        // failover's promote -- put a second database over it. A replica
        // node keeps every tenant it follows open.
        if t.repl
            .as_ref()
            .and_then(|r| r.follower())
            .is_some_and(|f| f.running())
        {
            return false;
        }
        // The node's stream holds a tenant open until it has sent what the
        // disk holds: the feed goes with the tenant, and writes it had not
        // sent would have come again as an image, a gap in the archive.
        // Writes not on the disk yet would reach it at the close's last
        // sync, after the stream let go of the feed: synced first, they are
        // waited for like the rest.
        let feed = t.repl.as_ref().and_then(|r| r.feed());
        if let Some(feed) = feed {
            let pending = {
                let g = t.read();
                g.is_dirty() && g.failure().is_none()
            };
            if pending {
                sync_now(t);
            }
            if feed.unsent() {
                return false;
            }
        }
        if checkpoint {
            let mut g = t.write();
            if g.stats().iter().any(|s| !s.vector_indexes.is_empty()) {
                if let Err(e) = g.checkpoint() {
                    crate::log!("could not write the checkpoint ({}): {e}", t.name);
                }
            }
        }
        if let (Some(streams), Some(feed)) = (&self.streams, feed) {
            let g = t.read();
            let place = (g.change_seq(), g.history().clone());
            drop(g);
            self.closed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(t.name.clone(), place);
            streams.detach(&t.name, feed);
        }
        *held = Held::Closed;
        true
    }

    /// Closes idle tenants, least recently used first, until the open ones
    /// fit under the ceiling or nothing more can be closed. Over the ceiling
    /// with everything busy, the open goes ahead anyway: refusing a read
    /// because other tenants are busy would turn a memory warning into an
    /// outage. A tenant in the middle of a write is counted as zero rather
    /// than waited for -- the ceiling is an early warning, not a guarantee.
    fn make_room(&self, just_opened: &str) {
        if self.max_memory == 0 {
            return;
        }
        let mut used = 0usize;
        let mut idle: Vec<(u64, String)> = Vec::new();
        for slot in self.all_slots() {
            let Ok(held) = slot.held.try_lock() else {
                continue;
            };
            let Held::Open(t) = &*held else { continue };
            used += t.db.try_read().map(|g| g.memory_bytes()).unwrap_or(0);
            if t.name != just_opened && Arc::strong_count(t) == 1 && !t.is_frozen() {
                idle.push((t.last_used.load(Ordering::Relaxed), t.name.clone()));
            }
        }
        idle.sort();
        for (_, name) in idle {
            if used < self.max_memory {
                break;
            }
            used = used.saturating_sub(self.with_slot(&name, |held| {
                let freed = match &*held {
                    Held::Open(t) => t.db.try_read().map(|g| g.memory_bytes()).unwrap_or(0),
                    Held::Closed => 0,
                };
                if self.close(held) {
                    freed
                } else {
                    0
                }
            }));
        }
    }

    /// The open tenants, cloned out of their slots. A slot that is busy
    /// opening or closing is skipped: it is not open yet, or not any more.
    fn open_tenants(&self) -> Vec<Arc<Tenant>> {
        self.all_slots()
            .iter()
            .filter_map(|s| match &*s.held.try_lock().ok()? {
                Held::Open(t) => Some(Arc::clone(t)),
                Held::Closed => None,
            })
            .collect()
    }

    /// Pushes every dirty open tenant to disk. The periodic syncer calls it,
    /// and does as the single database's does: the flush under the tenant's
    /// write lock, the fsync once it is let go. Held through the fsync, the
    /// lock kept every read and write of the tenant waiting out the disk
    /// once a pass: up to 27 ms at a time on macOS, against 1.8 this way,
    /// with a write every millisecond beside four readers.
    pub fn sync_dirty(&self) {
        for t in self.open_tenants() {
            // After a storage error nothing is clean again, and every pass
            // would log it; the writes that follow are refused and say so.
            let pending = {
                let g = t.read();
                g.is_dirty() && g.failure().is_none()
            };
            if !pending {
                continue;
            }
            sync_now(&t);
        }
    }

    /// Closes the tenants untouched for `idle` that nothing holds. A frozen
    /// tenant stays: the freeze lives on the open instance.
    pub fn close_idle(&self, idle: Duration) -> usize {
        let cutoff = self.now().saturating_sub(idle.as_millis() as u64);
        let names: Vec<String> = self
            .open_tenants()
            .iter()
            .filter(|t| t.last_used.load(Ordering::Relaxed) <= cutoff)
            .map(|t| t.name.clone())
            .collect();
        names
            .iter()
            .filter(|name| {
                self.with_slot(name, |held| match held {
                    Held::Open(t) if t.last_used.load(Ordering::Relaxed) <= cutoff => {
                        self.close(held)
                    }
                    _ => false,
                })
            })
            .count()
    }

    /// Stops writes to a tenant and waits for the ones in flight: once it
    /// returns, the database will not change until `thaw`.
    pub fn freeze(&self, name: &str) -> std::result::Result<(), Refused> {
        let t = self.get(name)?;
        t.frozen.store(true, Ordering::SeqCst);
        drop(t.gate.write().unwrap_or_else(|e| e.into_inner()));
        Ok(())
    }

    pub fn thaw(&self, name: &str) -> std::result::Result<(), Refused> {
        let t = self.get(name)?;
        t.frozen.store(false, Ordering::SeqCst);
        Ok(())
    }

    /// The tenant's whole image, graph included -- what a checkpoint would
    /// write, so the receiving node opens it without rebuilding anything.
    pub fn export(&self, name: &str) -> std::result::Result<Vec<u8>, Refused> {
        let t = self.get(name)?;
        let image = crate::held::read(&t.db).snapshot();
        Ok(image)
    }

    /// Installs an image as a new tenant. It is replayed in memory first, so
    /// a truncated or corrupt transfer is refused before it reaches the
    /// directory; the file then appears by rename, never half-written.
    pub fn import(&self, name: &str, image: &[u8]) -> std::result::Result<(), Refused> {
        check_name(name)?;
        let mut check = Database::new();
        let whole = check
            .load(image)
            .map_err(|e| Refused(400, format!("the image does not load: {e}")))?;
        if whole < image.len() {
            return Err(Refused(400, "the image is cut short".into()));
        }
        drop(check);

        self.with_slot(name, |held| {
            let path = self.path(name);
            if matches!(held, Held::Open(_)) || path.exists() {
                return Err(Refused(409, format!("tenant `{name}` already exists")));
            }
            // Its lock across the rename: a file put in place under a
            // shell or an app that has the name open would be written by
            // both.
            let _held = hold_tenant(name, &path)?;
            // A tenant installed here is its primary: a record left by a
            // replica of the same name deleted before would have it follow.
            let _ = std::fs::remove_file(self.follows_path(name));
            // Through a link, the file it leads to, as the open writes it,
            // and the image written beside that one: renamed over, the link
            // would be a file of its own.
            let target = fenec_core::fs::resolved(&path);
            let tmp = fenec_core::fs::beside(&target, "importing");
            let written = std::fs::File::create(&tmp).and_then(|mut f| {
                use std::io::Write;
                f.write_all(image)?;
                f.sync_all()
            });
            // A log a tenant of this name left (`fenec_core::fs`) is not
            // this image's: gone, durably, before the rename.
            let written = written
                .and_then(|_| fenec_core::fs::forget_sync_log(&target))
                .and_then(|_| std::fs::rename(&tmp, &target));
            if let Err(e) = written {
                let _ = std::fs::remove_file(&tmp);
                return Err(Refused(
                    500,
                    format!("could not write tenant `{name}`: {e}"),
                ));
            }
            self.forget_place(name);
            Ok(())
        })?;
        self.touch(name);
        Ok(())
    }

    /// Where a tenant whose file goes or is replaced stood is no more.
    fn forget_place(&self, name: &str) {
        self.closed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
    }

    /// Closes the tenant and removes its file. The streams on it are told to
    /// end; a request still running is waited for up to [`RELEASE_WAIT`].
    /// Requests for this tenant wait meanwhile; other tenants do not notice.
    pub fn delete(&self, name: &str) -> std::result::Result<(), Refused> {
        check_name(name)?;
        self.with_slot(name, |held| self.delete_held(name, held))?;
        self.touch(name);
        Ok(())
    }

    /// Deletes the copy `name` that refused a primary of another lineage
    /// than `expected` -- and only where, under the slot's lock the delete
    /// takes, it is still that copy: open as `opened`, the open whose
    /// follower refused, still following, its follower still refusing,
    /// `.follows` still naming `expected`. Checked before the slot was
    /// taken, a promotion of the copy or a move installing the tenant
    /// there landing between the check and the delete had the primary
    /// deleted. Whether it was deleted.
    pub(crate) fn delete_refused(
        &self,
        name: &str,
        opened: u64,
        expected: u32,
    ) -> std::result::Result<bool, Refused> {
        let deleted = self.with_slot(name, |held| {
            let Held::Open(t) = &*held else {
                return Ok(false);
            };
            let refusing = t.opened == opened
                && t.read().history().following
                && t.repl
                    .as_ref()
                    .and_then(|r| r.follower())
                    .is_some_and(|f| f.refused())
                && self.lineage_followed(name) == Some(expected);
            if !refusing {
                return Ok(false);
            }
            self.delete_held(name, held).map(|_| true)
        })?;
        if deleted {
            self.touch(name);
        }
        Ok(deleted)
    }

    fn delete_held(&self, name: &str, held: &mut Held) -> std::result::Result<(), Refused> {
        if let Held::Open(t) = &*held {
            release(t, name)?;
            // The last `Arc`: dropping it syncs, and the file is closed
            // before it is removed.
            *held = Held::Closed;
        }
        let path = self.path(name);
        if !path.exists() {
            return Err(Refused(404, format!("no tenant `{name}` on this node")));
        }
        // Not removed from under another process that has it open -- a
        // shell, an app -- which would write on into a file gone. The lock
        // file stays: removed, a process that opened it before the removal
        // and locked it after would hold a lock no later opener sees.
        let _held = hold_tenant(name, &path)?;
        std::fs::remove_file(&path)
            .map_err(|e| Refused(500, format!("could not remove tenant `{name}`: {e}")))?;
        // Once the file is gone: a delete that failed is no departure --
        // noted before, a removal refused had the archive say the tenant
        // went and came back 62 ms later, and a restore between left out
        // a tenant that never left. The stream, which looks for the file
        // without the slot, may find it gone a moment first, and takes
        // that moment.
        if self.streams.is_some() {
            let at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64);
            self.departed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .note(name, at);
        }
        let _ = std::fs::remove_file(path.with_extension("fenec.compacting"));
        // The sync log beside it (`fenec_core::fs`): a log names the
        // file it was written for, and is kept from a tenant made
        // again under the name that way too.
        let _ = fenec_core::fs::forget_sync_log(&path);
        let _ = std::fs::remove_file(self.follows_path(name));
        // A tenant made again under the name starts at the node's quota;
        // a router that keeps one sends it with the tenant.
        let _ = std::fs::remove_file(self.quota_path(name));
        self.forget_place(name);
        Ok(())
    }

    /// Every tenant on disk, sorted.
    pub fn names(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for e in entries.flatten() {
                let file = e.file_name();
                let Some(file) = file.to_str() else { continue };
                if let Some(name) = file.strip_suffix(".fenec") {
                    if check_name(name).is_ok() {
                        out.push(name.to_string());
                    }
                }
            }
        }
        out.sort();
        out
    }

    /// `(name, footprint)` of the open tenants and the bytes on disk of all
    /// of them -- what a router places new tenants by.
    /// The bytes a tenant holds on disk: its file and the sync log beside
    /// it, where one is kept.
    pub fn disk(&self, name: &str) -> u64 {
        let p = self.path(name);
        [fenec_core::fs::beside(&p, "sync"), p]
            .iter()
            .filter_map(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .sum()
    }

    pub fn stats(&self) -> Stats {
        let mut open: Vec<(String, usize, bool)> = self
            .open_tenants()
            .iter()
            .map(|t| (t.name.clone(), t.read().memory_bytes(), t.is_frozen()))
            .collect();
        open.sort();
        let unlinked = self
            .open_tenants()
            .iter()
            .map(|t| t.read().unlinked())
            .sum();
        let names = self.names();
        let disk = names.iter().map(|n| self.disk(n)).sum();
        Stats {
            tenants: names.len(),
            disk,
            open,
            unlinked,
            node_streams: self.streams.as_ref().map(|s| s.connected()),
        }
    }

    /// The shutdown path: no tenant opens from here on, and every open one
    /// is synced, checkpointed when it has a graph, and its slot and write
    /// lock are **kept** -- the caller exits with them held, so no write is
    /// accepted between the last sync and exit. Returns how many were open.
    pub fn shutdown(&self) -> usize {
        self.shutdown_with(true)
    }

    /// [`Tenants::shutdown`], checkpointing only where `checkpoint` and the
    /// node's own setting both say so: a node that stops before it served,
    /// or on a failure, writes none, since a checkpoint rewrites a tenant's
    /// whole file, and a node failing to start under a supervisor's
    /// restarts rewrote its tenants' files at each one.
    pub fn shutdown_with(&self, checkpoint: bool) -> usize {
        // The flag first, the snapshot after: an open that got past the flag
        // inserted its slot before the snapshot, and is waited for below.
        self.shutting_down.store(true, Ordering::SeqCst);
        let mut count = 0;
        for slot in self.all_slots() {
            let held = slot.held.lock().unwrap_or_else(|e| e.into_inner());
            if let Held::Open(t) = &*held {
                count += 1;
                let mut g = t.write();
                if g.is_dirty() {
                    if let Err(e) = g.sync() {
                        crate::log!("sync error ({}): {e}", t.name);
                    }
                }
                let graphs = g.stats().iter().any(|s| !s.vector_indexes.is_empty());
                if checkpoint && self.checkpoint && graphs {
                    if let Err(e) = g.checkpoint() {
                        crate::log!("could not write the checkpoint ({}): {e}", t.name);
                    }
                }
                // Leaked on purpose: the process is about to exit, and a lock
                // released here would let a session write after the final sync.
                std::mem::forget(g);
            }
            std::mem::forget(held);
        }
        count
    }
}

/// Pushes a tenant's writes to disk as the syncer does: the flush under its
/// write lock, the fsync once it is let go, and no wait for the streams to
/// send them (`replication::unwaited`): no client's answer waits on the
/// syncer or a close, and the wait held each tenant of a pass, and a close
/// under its slot's lock, a `SENT_WAIT` for a stream busy elsewhere.
fn sync_now(t: &Tenant) {
    let flushed = t.write().flush();
    match flushed {
        Ok(Some(durability)) => {
            if let Err(e) = crate::replication::unwaited(durability) {
                crate::log!("sync error ({}): {e}", t.name);
                t.write().fail(&e);
            }
        }
        Ok(None) => {}
        Err(e) => crate::log!("sync error ({}): {e}", t.name),
    }
}

/// A copy that refused a primary of another lineage -- its tenant deleted
/// and made again under the name -- deletes itself, file and `.follows`:
/// it is derived data, and its primary is another tenant's or gone.
/// Stopped and kept, it held the deleted tenant's rows on a node of a
/// region nobody had chosen for the new one, for anyone reading the node
/// to find. On a thread of its own -- the follower calling this has to end
/// before the delete can close the tenant -- and only where the tenant is
/// still the copy that refused, as the delete finds it
/// ([`Tenants::delete_refused`]).
fn refused_copy(me: Option<std::sync::Weak<Tenants>>, name: &str, opened: u64, expected: u32) {
    let Some(tenants) = me.and_then(|w| w.upgrade()) else {
        crate::log!(
            "tenant `{name}`'s copy here refused another tenant of its name, and stays until \
             deleted"
        );
        return;
    };
    let who = name.to_string();
    let spawned = std::thread::Builder::new()
        .name("fenec-refused-copy".into())
        .spawn(move || {
            refusals::pass(&who);
            match tenants.delete_refused(&who, opened, expected) {
                Ok(true) => {
                    COPIES_REFUSED.fetch_add(1, Ordering::Relaxed);
                    crate::log!(
                        "tenant `{who}`'s copy here followed a primary of another lineage -- the \
                         name made again -- and is deleted"
                    )
                }
                // Promoted, installed anew, told another lineage or
                // closed meanwhile: no longer the copy that refused.
                Ok(false) => {}
                Err(e) => crate::log!(
                    "tenant `{who}`'s copy here refused another tenant of its name, and stayed: {}",
                    e.1
                ),
            }
            refusals::done(&who);
        });
    if let Err(e) = spawned {
        crate::log!(
            "tenant `{name}`'s copy here refused another tenant of its name, and stayed: {e}"
        );
    }
}

/// A test's stand-in for a holder that took a tenant's database after its
/// close looked ([`Tenant::db_in_use`]) -- the `@ttl` sweeper or the graph
/// keeper upgrading its `Weak` between the look and the drop -- which then
/// writes the file past the close: a close of a name listed here closes
/// past what holds the database. Tests list names of their own, the
/// process's being shared.
#[doc(hidden)]
pub mod late_holders {
    use std::sync::{Mutex, MutexGuard};

    static NAMES: Mutex<Vec<String>> = Mutex::new(Vec::new());

    fn names() -> MutexGuard<'static, Vec<String>> {
        NAMES.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Closes the tenant `name` past its database's holders until
    /// [`forget`].
    pub fn close_past(name: &str) {
        names().push(name.to_string());
    }

    pub fn forget(name: &str) {
        names().retain(|n| n != name);
    }

    pub(super) fn listed(name: &str) -> bool {
        let names = names();
        !names.is_empty() && names.iter().any(|n| n == name)
    }
}

/// A test's hold on a refused copy's delete, by the tenant's name: the
/// thread waits at [`pass`] until let go, so that a promotion or an install
/// lands between the refusal and the delete. Names no test holds pass at
/// once; tests hold names of their own, the process's being shared.
#[doc(hidden)]
pub mod refusals {
    use std::collections::HashMap;
    use std::sync::{Condvar, Mutex};

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum Stage {
        /// Held by a test, no copy of the name refused yet.
        Held,
        /// A refusal waits at the hold, its delete not asked.
        Waiting,
        /// Let go.
        Let,
        /// Its delete asked, done or not.
        Done,
    }

    static STAGES: Mutex<Option<HashMap<String, Stage>>> = Mutex::new(None);
    static MOVED: Condvar = Condvar::new();

    fn set(name: &str, from: Option<Stage>, to: Stage) {
        let mut g = STAGES.lock().unwrap_or_else(|e| e.into_inner());
        let map = g.get_or_insert_with(HashMap::new);
        if from.is_none() || map.get(name).copied() == from {
            map.insert(name.to_string(), to);
            MOVED.notify_all();
        }
    }

    /// Holds the next refused copy of `name` before its delete.
    pub fn hold(name: &str) {
        set(name, None, Stage::Held);
    }

    /// Lets it go.
    pub fn release(name: &str) {
        set(name, None, Stage::Let);
    }

    /// Waits until `name`'s refusal reaches `stage`, up to `within`.
    pub fn wait_for(name: &str, stage: Stage, within: std::time::Duration) -> bool {
        let until = std::time::Instant::now() + within;
        let mut g = STAGES.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if g.as_ref().and_then(|m| m.get(name)) == Some(&stage) {
                return true;
            }
            let left = until.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return false;
            }
            g = MOVED
                .wait_timeout(g, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    pub(crate) fn pass(name: &str) {
        let mut g = STAGES.lock().unwrap_or_else(|e| e.into_inner());
        let held = g.as_ref().and_then(|m| m.get(name)) == Some(&Stage::Held);
        if !held {
            return;
        }
        g.get_or_insert_with(HashMap::new)
            .insert(name.to_string(), Stage::Waiting);
        MOVED.notify_all();
        while g.as_ref().and_then(|m| m.get(name)) == Some(&Stage::Waiting) {
            g = MOVED.wait(g).unwrap_or_else(|e| e.into_inner());
        }
    }

    pub(crate) fn done(name: &str) {
        set(name, Some(Stage::Let), Stage::Done);
    }
}

/// Ends what holds an open tenant besides its slot -- its subscriptions, the
/// replicas it feeds and its follower -- and waits up to [`RELEASE_WAIT`]
/// for a request still running. A replica's stream holds the tenant as a
/// subscription does, and is ended for the same reason. 409 when something
/// does not let go, with everything started again: left ended, the tenant
/// took writes its replicas were never sent.
fn release(t: &Arc<Tenant>, name: &str) -> std::result::Result<(), Refused> {
    t.hub.close();
    if let Some(repl) = &t.repl {
        repl.close_streams();
    }
    let deadline = Instant::now() + RELEASE_WAIT;
    while Arc::strong_count(t) > 1 {
        if Instant::now() >= deadline {
            t.hub.reopen();
            if let Some(repl) = &t.repl {
                repl.reopen_streams(format!("fenec-replica-{name}"));
            }
            return Err(Refused(409, format!("tenant `{name}` is still in use")));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

pub struct Stats {
    pub tenants: usize,
    pub disk: u64,
    /// `(name, memory_bytes, frozen)`
    pub open: Vec<(String, usize, bool)>,
    /// The open tenants' vectors still to be linked into their graphs
    /// (`link::beside`).
    pub unlinked: usize,
    /// The node's streams of every tenant connected now, on a node that
    /// replicates its tenants: its archivers.
    pub node_streams: Option<usize>,
}

/// What a tenant uses of its quota, as `quota_report` writes it.
fn usage_json(
    bytes: u64,
    rows: Option<u64>,
    streams: Option<u64>,
    requests: u64,
    held: Option<u64>,
) -> String {
    let opt = |v: Option<u64>| v.map_or("null".to_string(), |n| n.to_string());
    format!(
        "{{\"bytes\":{bytes},\"rows\":{},\"streams\":{},\"requests\":{requests},\"held\":{}}}",
        opt(rows),
        opt(streams),
        opt(held)
    )
}

/// `[a-z0-9_-]{1,64}`: safe as a file name everywhere, no traversal, and
/// the same name on a case-insensitive file system.
pub fn check_name(name: &str) -> std::result::Result<(), Refused> {
    let ok = !name.is_empty()
        && name.len() <= MAX_NAME
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(Refused(
            400,
            format!("invalid tenant name `{name}`: [a-z0-9_-], 1 to {MAX_NAME} characters"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fenec-registry-{tag}-{}-{:?}",
            std::process::id(),
            Instant::now()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn run(t: &Tenant, sql: &str) {
        let stmt = fenec_ql::parse_one(sql).unwrap();
        t.db.write().unwrap().execute(&stmt).unwrap();
    }

    #[test]
    fn a_missing_tenant_leaves_nothing_in_the_registry() {
        let dir = scratch("missing");
        let reg = Tenants::new(&dir).unwrap();
        for i in 0..100 {
            let Err(Refused(status, _)) = reg.get(&format!("nope{i}")) else {
                panic!("a tenant that is not on disk opened");
            };
            assert_eq!(status, 404);
        }
        assert!(reg.registry().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// An open that takes long -- a large file without its graph -- holds
    /// up no other tenant: an open one is handed out while the slow open is
    /// held at a gate in its setup, for as long as the test keeps it there.
    /// The open slept 400 ms and the other tenant's `get` was bounded at
    /// 100 ms, which measured the machine as much as the registry: the
    /// sleep before it gave the slow open 50 ms to begin, and a get
    /// throttled for longer than that failed with no open in its way.
    #[test]
    fn a_slow_open_does_not_stall_other_tenants() {
        #[derive(Default)]
        struct Gate {
            armed: bool,
            entered: bool,
            released: bool,
        }
        let gate = Arc::new((Mutex::new(Gate::default()), std::sync::Condvar::new()));
        let dir = scratch("slow");
        let g = Arc::clone(&gate);
        let reg = Arc::new(Tenants::new(&dir).unwrap().with_setup(move |_| {
            let (m, cv) = &*g;
            let mut s = m.lock().unwrap();
            if s.armed {
                s.entered = true;
                cv.notify_all();
                while !s.released {
                    s = cv.wait(s).unwrap();
                }
            }
            Ok(())
        }));
        reg.create("fast").unwrap();
        reg.create("slow").unwrap();
        assert_eq!(reg.close_idle(Duration::ZERO), 1 + 1);
        drop(reg.get("fast").unwrap());

        // The next open is held in its setup until released.
        gate.0.lock().unwrap().armed = true;
        let opener = Arc::clone(&reg);
        let slow = std::thread::spawn(move || opener.get("slow").map(|_| ()));
        {
            let (m, cv) = &*gate;
            let (_s, waited) = cv
                .wait_timeout_while(m.lock().unwrap(), Duration::from_secs(30), |s| !s.entered)
                .unwrap();
            assert!(!waited.timed_out(), "the slow open never reached its setup");
        }
        // Asked on a thread of its own: a get that waited for the held
        // open would wait for good, and is seen not answering instead.
        let (tx, rx) = std::sync::mpsc::channel();
        let asker = Arc::clone(&reg);
        std::thread::spawn(move || {
            let _ = tx.send(asker.get("fast").map(|_| ()));
        });
        let fast = rx.recv_timeout(Duration::from_secs(30));
        // Let go only now, whatever the answer: a failure leaves no thread
        // held behind it.
        {
            let (m, cv) = &*gate;
            m.lock().unwrap().released = true;
            cv.notify_all();
        }
        assert!(
            matches!(fast, Ok(Ok(()))),
            "an open tenant waited for another one's open: {fast:?}"
        );
        slow.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn closing_and_reopening_under_load_loses_nothing() {
        // Writers and a closer racing: a second instance of the file would
        // show up as lost or duplicated documents. A tenant some writer
        // holds is not idle, and four writers back to back left the closer
        // no moment with none of them holding it -- under load it closed
        // nothing at all -- so the writers also stop together now and then,
        // and the tenant is closed there before all four open it again at
        // once. The closer races them beside that, whenever it gets a turn.
        const ROUNDS: usize = 4;
        let dir = scratch("race");
        let reg = Arc::new(Tenants::new(&dir).unwrap());
        run(
            &reg.create("t").unwrap(),
            "create collection n (w int, i int)",
        );

        let stop = Arc::new(AtomicBool::new(false));
        let closer = {
            let (reg, stop) = (Arc::clone(&reg), Arc::clone(&stop));
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    reg.close_idle(Duration::ZERO);
                }
            })
        };
        let parked = Arc::new(std::sync::Barrier::new(4));
        let writers: Vec<_> = (0..4)
            .map(|w| {
                let (reg, parked) = (Arc::clone(&reg), Arc::clone(&parked));
                std::thread::spawn(move || {
                    for round in 0..ROUNDS {
                        for i in 0..50 {
                            let t = reg.get("t").unwrap();
                            run(&t, &format!("put n {{w: {w}, i: {}}}", round * 50 + i));
                        }
                        // None of the four holds the tenant now: it closes,
                        // here or in the closer, whose look may hold it a
                        // moment longer.
                        if parked.wait().is_leader() {
                            while !reg.with_slot("t", |held| {
                                reg.close(held);
                                matches!(held, Held::Closed)
                            }) {}
                        }
                        parked.wait();
                    }
                })
            })
            .collect();
        for w in writers {
            w.join().unwrap();
        }
        stop.store(true, Ordering::SeqCst);
        closer.join().unwrap();

        reg.close_idle(Duration::ZERO);
        let db = fenec_core::fs::open(dir.join("t.fenec")).unwrap();
        let stmt = fenec_ql::parse_one("get n count").unwrap();
        let Ok(Response::Rows(rs)) = db.query(&stmt, &[]) else {
            panic!("count")
        };
        assert_eq!(rs.rows[0].values[0], Value::Int(800));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_closed_tenant_with_a_sync_log_has_no_place_read() {
        let dir = scratch("peek");
        let reg = Tenants::new(&dir).unwrap().with_replication(Replicated {
            token: "r".into(),
            buffer: 1 << 20,
            upstream: None,
            sync_on_write: true,
        });
        run(&reg.create("t").unwrap(), "create collection n (i int)");
        assert_eq!(reg.close_idle(Duration::ZERO), 1);
        let (seq, _) = reg.peek("t").unwrap().expect("a closed tenant read");
        assert_eq!(seq, 1);
        // A log a power loss left beside it: its durable writes are in the
        // log, not yet in the file, so the file's change says nothing.
        std::fs::write(
            fenec_core::fs::beside(&dir.join("t.fenec"), "sync"),
            b"FENECSYN durable writes the file lacks",
        )
        .unwrap();
        assert!(reg.peek("t").unwrap().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    fn replicating(dir: &Path) -> Tenants {
        Tenants::new(dir).unwrap().with_replication(Replicated {
            token: "r".into(),
            buffer: 1 << 20,
            upstream: None,
            sync_on_write: true,
        })
    }

    /// A tenant whose database something beside the queries holds -- a
    /// compact or a warm-up for its whole run, as `link::compact` and
    /// `warm` hold it -- is not closed when idle: closed, the database
    /// outlived it with the file's lock, and each request's open waited
    /// `RELEASE_WAIT` for it, then answered 500. Answered from the instance
    /// still open, and closed once let go.
    #[test]
    fn a_tenant_whose_database_is_in_use_is_not_closed() {
        let dir = scratch("in-use");
        let reg = Tenants::new(&dir).unwrap();
        let t = reg.create("t").unwrap();
        run(&t, "create collection n (i int)");
        let compacting = Arc::clone(&t.db);
        drop(t);
        assert_eq!(reg.close_idle(Duration::ZERO), 0);
        let again = reg.get("t").unwrap();
        assert!(Arc::ptr_eq(&again.db, &compacting), "opened again");
        drop(again);
        drop(compacting);
        assert_eq!(reg.close_idle(Duration::ZERO), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A follow lets the tenant go and opens its file again, while a thread
    /// beside the queries -- a linking slice, the graph keeper's append, a
    /// compact -- still holds the database, and with it the file's lock:
    /// the open waits for it, held here at a gate until the open is seen
    /// waiting (`lock::waiting_for`), and goes on once it lets go, rather
    /// than answer 500 or open a second database over the file.
    #[test]
    fn a_reopen_waits_for_what_still_holds_the_database() {
        let dir = scratch("reopen");
        let reg = Arc::new(replicating(&dir));
        let t = reg.create("t").unwrap();
        run(&t, "create collection n (i int)");
        run(&t, "put n {i: 1}");
        let (open_gate, gate) = std::sync::mpsc::channel::<()>();
        let holder = {
            let db = Arc::clone(&t.db);
            std::thread::spawn(move || {
                gate.recv().unwrap();
                drop(db);
            })
        };
        drop(t);
        let follow = {
            let reg = Arc::clone(&reg);
            std::thread::spawn(move || reg.follow("t", Some("http://127.0.0.1:9")))
        };
        let path = dir.join("t.fenec");
        while !fenec_abi::lock::waiting_for(&path) {
            if follow.is_finished() {
                panic!("not waited for: {:?}", follow.join().unwrap());
            }
            std::thread::yield_now();
        }
        open_gate.send(()).unwrap();
        holder.join().unwrap();
        follow.join().unwrap().unwrap();
        let t = reg.get("t").unwrap();
        assert!(t.read().history().following);
        let stmt = fenec_ql::parse_one("get n count").unwrap();
        let Ok(Response::Rows(rs)) = t.read().query(&stmt, &[]) else {
            panic!("count")
        };
        assert_eq!(rs.rows[0].values[0], Value::Int(1));
        if let Some(f) = t.repl.as_ref().and_then(|r| r.follower()) {
            f.halt();
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A delete is a departure only on a node that replicates its tenants,
    /// where an archiver can ask when one went: elsewhere the names were
    /// kept for nothing.
    #[test]
    fn departures_are_kept_where_an_archiver_can_ask() {
        let dir = scratch("departed");
        let reg = replicating(&dir);
        drop(reg.create("t").unwrap());
        reg.delete("t").unwrap();
        assert!(reg.departed("t").is_some());
        let plain = Tenants::new(dir.join("plain")).unwrap();
        drop(plain.create("t").unwrap());
        plain.delete("t").unwrap();
        assert!(plain.departed("t").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A delete whose file could not be removed is no departure: noted as
    /// one, the archive said the tenant went and came back 62 ms later, and
    /// a restore to a moment between left out a tenant that never left.
    #[cfg(unix)]
    #[test]
    fn a_delete_that_fails_is_no_departure() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("delete-fails");
        let reg = replicating(&dir);
        drop(reg.create("t").unwrap());
        let mode = |m| std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(m));
        mode(0o555).unwrap();
        let deleted = reg.delete("t");
        mode(0o755).unwrap();
        // Run as root, the directory refuses nothing: no failure to hold.
        if deleted.is_err() {
            assert!(dir.join("t.fenec").exists());
            assert_eq!(reg.departed("t"), None);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn names_are_file_safe() {
        assert!(check_name("acme").is_ok());
        assert!(check_name("a-b_9").is_ok());
        assert!(check_name("").is_err());
        assert!(check_name("..").is_err());
        assert!(check_name("a/b").is_err());
        assert!(check_name("Acme").is_err());
        assert!(check_name(&"a".repeat(MAX_NAME + 1)).is_err());
    }
}
