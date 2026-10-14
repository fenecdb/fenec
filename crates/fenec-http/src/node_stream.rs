//! Every tenant of a node over one stream: what archives a node.
//!
//! A tenant's own stream (`/t/<tenant>/_replication`) is a connection and a
//! thread of the node's for as long as it runs, one of `--max-connections`,
//! and it holds its tenant open the while: archived that way, a node of a
//! thousand tenants held a thousand of each, every tenant open for good --
//! and with the default of 100 connections it could not be archived at all.
//! Most of a node's tenants are idle, so `POST /_replication/tenants` is one
//! connection and one thread for all of them, and holds none of them open:
//! it sends a tenant's writes from its feed while the tenant is open, and
//! nothing while it is closed, whose file does not change.
//!
//! **A tenant closes only once the stream has sent what its disk holds.**
//! The feed goes with the tenant, and writes the stream had not sent would
//! have had to come again as an image, leaving the archive a gap. So the
//! stream is a sender on each tenant's feed as a replica's stream is -- put
//! there as the tenant opens (`NodeStreams::attach`), where it stands where
//! the file does, and on the tenants open as it connects -- and `Tenants`
//! closes a tenant only when no sender has a write left to send
//! (`Feed::unsent`); it looks again at the next pass, never waiting.
//!
//! **One stream holds up no tenant for another.** Under `--sync always` a
//! durable write waits for the stream as for a replica -- while the stream
//! keeps up. Whatever can keep it busy -- reading a closed tenant's file,
//! opening one, waiting for a tenant's lock (`Listener::read`), an image's
//! snapshot and its sending, a socket the client reads slower than it is
//! written -- it does held up (`Listener::hold`),
//! and a write waits for no stream held up: each tenant's stream stalled its
//! own tenant alone, and one stream for all of them held every tenant's
//! next durable write for its `SENT_WAIT`, 1.008 s, while it opened another
//! one. Nor does an image wait for its tenant's disk: it is kept, and sent
//! once the disk holds it, the stream going on meanwhile (`Pending`) --
//! waited for there, up to 30 s, every other tenant waited with it. The
//! syncer and a close wait for no stream at all (`replication::unwaited`):
//! no client's answer waits on them. So an archive of a node is synchronous
//! only while the stream keeps up: a write answered while it is held up or
//! behind is on the node's disk and not yet in the archive, sent later --
//! at worst inside an image -- and lacking from it if that disk is lost.
//!
//! **Which tenants have news is told, not polled.** A tenant's feed tells
//! the node's streams as its writes reach the disk, it starts over or it
//! closes (`Feed::tell_with`), a tenant made, installed, deleted, promoted
//! or made to follow tells them too, and a stream looks only at the tenants
//! it was told of -- and at the directory every [`RESCAN`], for a file put
//! there by hand. With no stream connected a tenant's sync pays two loads.
//! A tenant released from under the stream -- deleted, made to follow --
//! is looked at again until it is gone or back.
//!
//! **Where a closed tenant stands is known without opening it**: where it
//! closed in this process, or -- the first stream after the node starts --
//! read from its file (`Tenants::peek`): 1 001 tenants in 0.29-0.33 s, where
//! an open, which fsyncs the file, was 6.3 ms a tenant. A closed tenant the
//! stream is told of is read again rather than taken at its close: a holder
//! of its database -- the `@ttl` sweeper -- may have written it since. Only a
//! tenant the client is behind in is opened, for its image, and closed
//! again.
//!
//! ## Wire format
//!
//! The request's body says where the client stands, a line a tenant it
//! holds: `<tenant> <since> <history, hex> <image>`, the last 1 when it holds
//! no image to start from. The answer is the replication stream's messages,
//! `[tag: u8][length: u64 LE][payload]`, a tenant's led by its name,
//! `[length: u8][name]`:
//!
//! ```text
//! N  node     [version: 1]
//! H  hello    [name][the tenant stream's hello]: where it goes on, an image
//!             following when it says so
//! I  image    [name][the image]
//! W  writes   [name][the tenant stream's writes]
//! L  lineage  [name][history]: the history the tenant's writes are on
//!             moved -- a promotion -- and the writes after it come after it
//! G  gone     [name][time_ms u64]: no longer on this node, deleted or
//!             moved to another -- when, by the node's clock: when this
//!             process deleted it, or when the stream saw it go
//! K  alive    [time_ms u64]
//! E  end      why, in UTF-8
//! ```
//!
//! Every tenant on the node is said hello to, an image sent where the
//! client's place does not lead to its writes -- a tenant new to it, one it
//! fell behind, one restored -- and every tenant it lists that the node does
//! not hold is gone. A stream's writes are sent once they are on the
//! tenant's disk, as a replica's are.

use crate::http::{Method, Request, Response};
use crate::replication::{message, u64s, Feed, Next, ALIVE, MESSAGE, SILENCE, VERSION};
use crate::tenants::{check_name, Refused, Seen, Tenant, Tenants};
use fenec_core::history::History;
use fenec_core::prelude::Database;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{self, BufWriter, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock, RwLockReadGuard, TryLockError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How often a stream reads the directory again, for a tenant's file that
/// came or went with nothing telling it: an operator's copy, a restore.
/// Every change made through the node is told at once.
pub const RESCAN: Duration = Duration::from_secs(10);

/// How soon a tenant is looked at again that was busy opening or closing,
/// released from under the stream, or whose image its disk did not hold
/// yet.
const RETRY: Duration = Duration::from_millis(50);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// The node's streams of every tenant connected now: what a tenant's feed
/// tells of its writes, and what a tenant consults as it opens and closes.
pub struct NodeStreams {
    listeners: Mutex<Vec<Arc<Listener>>>,
    /// How many are connected, read without the lock: with none, a feed
    /// telling of a write reads this and goes.
    count: AtomicUsize,
}

/// One stream's part of the registry.
pub(crate) struct Listener {
    key: u64,
    state: Mutex<State>,
    cv: Condvar,
    /// Set while the stream is held up elsewhere ([`Listener::hold`]): no
    /// write waits for it, on any feed it sends from.
    held: Arc<AtomicBool>,
}

#[derive(Default)]
struct State {
    /// The tenants the client holds: where it stands in each.
    following: HashMap<String, Followed>,
    /// The feed each open tenant's writes are sent from, and its epoch
    /// when this stream took it.
    feeds: HashMap<String, (Arc<Feed>, u64)>,
    /// Tenants told of since the stream last looked.
    dirty: BTreeSet<String>,
}

/// Where the client stands in a tenant.
#[derive(Clone, Copy, Debug)]
struct Followed {
    /// The last write it holds, and the history it was sent on.
    cursor: u64,
    history: u64,
    /// It holds no image to start from, so it is sent one.
    image: bool,
}

/// Whether a client standing at `followed` holds a tenant at `seq` on
/// `history` whole.
fn stands_at(followed: Option<Followed>, seq: u64, history: &History) -> bool {
    followed.is_some_and(|f| !f.image && f.cursor == seq && history.continues(f.history, seq, seq))
}

impl NodeStreams {
    pub fn new() -> Arc<NodeStreams> {
        Arc::new(NodeStreams {
            listeners: Mutex::new(Vec::new()),
            count: AtomicUsize::new(0),
        })
    }

    /// The streams connected now.
    pub fn connected(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }

    fn join(&self, following: HashMap<String, Followed>) -> Arc<Listener> {
        let l = Arc::new(Listener {
            key: crate::replication::fresh_id(),
            state: Mutex::new(State {
                following,
                ..State::default()
            }),
            cv: Condvar::new(),
            held: Arc::new(AtomicBool::new(false)),
        });
        let mut all = lock(&self.listeners);
        all.push(Arc::clone(&l));
        self.count.store(all.len(), Ordering::Relaxed);
        l
    }

    /// A stream ended: off every feed it sent from, so no durable write
    /// waits for it and no tenant stays open for it.
    fn leave(&self, l: &Arc<Listener>) {
        let mut all = lock(&self.listeners);
        all.retain(|x| !Arc::ptr_eq(x, l));
        self.count.store(all.len(), Ordering::Relaxed);
        drop(all);
        for (_, (feed, _)) in lock(&l.state).feeds.drain() {
            feed.stopped(l.key);
        }
    }

    /// Tells every stream that `name` has news: writes on its disk, a feed
    /// started over or closed, the tenant made, gone or promoted.
    pub(crate) fn touch(&self, name: &str) {
        if self.count.load(Ordering::Relaxed) == 0 {
            return;
        }
        for l in lock(&self.listeners).iter() {
            l.mark(name);
        }
    }

    /// A tenant opened with `feed`, at change `seq` on `history`: its feed
    /// tells of it from now on, and each stream whose client stands where
    /// the file does sends from it -- put on it before the tenant takes a
    /// write, so the tenant stays open until the write is sent. A stream
    /// that stands elsewhere is told to look.
    pub(crate) fn attach(
        self: &Arc<Self>,
        name: &str,
        feed: &Arc<Feed>,
        seq: u64,
        history: &History,
    ) {
        feed.tell_with(self, name);
        if self.count.load(Ordering::Relaxed) == 0 {
            return;
        }
        for l in lock(&self.listeners).iter() {
            let mut st = lock(&l.state);
            // A feed the stream holds of an earlier opening is dead: kept,
            // the stream read nothing from it again and sent this one's
            // writes never -- an image pending across a close and an open
            // put the old feed back once it went out.
            if st
                .feeds
                .get(name)
                .is_some_and(|(old, _)| !Arc::ptr_eq(old, feed))
            {
                if let Some((old, _)) = st.feeds.remove(name) {
                    old.stopped(l.key);
                }
            }
            if stands_at(st.following.get(name).copied(), seq, history) {
                feed.sending_as(l.key, seq, Some(&l.held), false);
                if let Some((old, _)) = st
                    .feeds
                    .insert(name.into(), (Arc::clone(feed), feed.epoch()))
                {
                    if !Arc::ptr_eq(&old, feed) {
                        old.stopped(l.key);
                    }
                }
            } else {
                st.dirty.insert(name.into());
                l.cv.notify_one();
            }
        }
    }

    /// A tenant closed, its feed going with it: no stream sends from it.
    pub(crate) fn detach(&self, name: &str, feed: &Arc<Feed>) {
        if self.count.load(Ordering::Relaxed) == 0 {
            return;
        }
        for l in lock(&self.listeners).iter() {
            l.unfeed(name, feed);
        }
    }
}

/// The stream held up ([`Listener::hold`]) until dropped.
struct Hold<'a> {
    l: &'a Listener,
    /// Held already when this began: an inner hold lets go of nothing.
    was: bool,
}

impl Drop for Hold<'_> {
    fn drop(&mut self) {
        if !self.was {
            self.l.held.store(false, Ordering::SeqCst);
        }
    }
}

impl Listener {
    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    /// Holds the stream up -- it is about to do what can take long -- until
    /// the guard goes: no write waits for it meanwhile, and one waiting
    /// already is woken to stop.
    fn hold(&self) -> Hold<'_> {
        let was = self.held.swap(true, Ordering::SeqCst);
        if !was {
            for (feed, _) in self.state().feeds.values() {
                feed.wake_waits();
            }
        }
        Hold { l: self, was }
    }

    /// `db`'s read lock, the stream held up while it waits for it: a
    /// tenant's write lock is held as long as its longest write -- a
    /// `/batch`, a compact under the lock -- and waited for unheld, every
    /// other tenant's next durable write waited out `SENT_WAIT` behind it,
    /// 1.014 s.
    fn read<'d>(&self, db: &'d RwLock<Database>) -> RwLockReadGuard<'d, Database> {
        match db.try_read() {
            Ok(g) => g,
            Err(TryLockError::Poisoned(e)) => e.into_inner(),
            Err(TryLockError::WouldBlock) => {
                let _held = self.hold();
                crate::held::read(db)
            }
        }
    }

    fn mark(&self, name: &str) {
        let mut st = self.state();
        if !st.dirty.contains(name) {
            st.dirty.insert(name.into());
        }
        drop(st);
        self.cv.notify_one();
    }

    /// The tenants told of, once one is or `wait` has passed.
    fn take(&self, wait: Duration) -> BTreeSet<String> {
        let mut st = self.state();
        if st.dirty.is_empty() {
            st = self
                .cv
                .wait_timeout(st, wait)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        std::mem::take(&mut st.dirty)
    }

    fn followed(&self, name: &str) -> Option<Followed> {
        self.state().following.get(name).copied()
    }

    /// The client now stands at `cursor` on `history` in `name`.
    fn follow(&self, name: &str, cursor: u64, history: u64) {
        self.state().following.insert(
            name.into(),
            Followed {
                cursor,
                history,
                image: false,
            },
        );
    }

    fn moved(&self, name: &str, cursor: u64) {
        if let Some(f) = self.state().following.get_mut(name) {
            f.cursor = cursor;
        }
    }

    fn rehistory(&self, name: &str, history: u64) {
        if let Some(f) = self.state().following.get_mut(name) {
            f.history = history;
        }
    }

    fn feed(&self, name: &str, feed: &Arc<Feed>, epoch: u64) {
        let old = self
            .state()
            .feeds
            .insert(name.into(), (Arc::clone(feed), epoch));
        if let Some((old, _)) = old {
            if !Arc::ptr_eq(&old, feed) {
                old.stopped(self.key);
            }
        }
    }

    fn unfeed(&self, name: &str, feed: &Arc<Feed>) {
        let mut st = self.state();
        if st
            .feeds
            .get(name)
            .is_some_and(|(f, _)| Arc::ptr_eq(f, feed))
        {
            st.feeds.remove(name);
            drop(st);
            feed.stopped(self.key);
        }
    }

    /// The client no longer follows `name`: whether it did.
    fn unfollow(&self, name: &str) -> bool {
        let mut st = self.state();
        let was = st.following.remove(name).is_some();
        let fed = st.feeds.remove(name);
        drop(st);
        if let Some((feed, _)) = fed {
            feed.stopped(self.key);
        }
        was
    }
}

/// Takes a finished stream out of the registry.
struct Leave<'a>(&'a NodeStreams, &'a Arc<Listener>);
impl Drop for Leave<'_> {
    fn drop(&mut self) {
        self.0.leave(self.1);
    }
}

/// The client's places, a line a tenant: `<tenant> <since> <history hex>
/// <image 0|1>`.
fn places(body: &[u8]) -> Result<HashMap<String, Followed>, String> {
    let text = std::str::from_utf8(body).map_err(|_| "the body is not UTF-8".to_string())?;
    let mut out = HashMap::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let bad = || format!("`{line}` is not `<tenant> <since> <history hex> <image 0|1>`");
        let mut it = line.split_ascii_whitespace();
        let (Some(name), Some(since), Some(history), Some(image), None) =
            (it.next(), it.next(), it.next(), it.next(), it.next())
        else {
            return Err(bad());
        };
        check_name(name).map_err(|e| e.1)?;
        let followed = Followed {
            cursor: since.parse().map_err(|_| bad())?,
            history: u64::from_str_radix(history, 16).map_err(|_| bad())?,
            image: match image {
                "0" => false,
                "1" => true,
                _ => return Err(bad()),
            },
        };
        out.insert(name.to_string(), followed);
    }
    Ok(out)
}

/// `/_replication/...` on a node of tenants: the stream of every tenant,
/// with the node's replication token. `None` when the stream took the
/// connection over and has ended.
pub fn handle(out: &mut TcpStream, tenants: &Tenants, req: &Request) -> Option<Response> {
    let given = req
        .header("authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    let ok = tenants
        .replication_token()
        .is_some_and(|t| crate::constant_eq(given.as_bytes(), t.as_bytes()));
    if !ok {
        return Some(
            Response::error(401, "invalid or missing replication token")
                .header("WWW-Authenticate", "Bearer"),
        );
    }
    crate::audit::verified();
    match (req.method, req.segments().as_slice()) {
        (Method::Post, ["_replication", "tenants"]) => {}
        _ => {
            return Some(Response::error(
                404,
                "a node's replication is POST /_replication/tenants, a tenant's \
                 /t/<tenant>/_replication",
            ))
        }
    }
    let Some(streams) = tenants.node_streams() else {
        return Some(Response::error(
            409,
            "this node replicates nothing (--replication-token)",
        ));
    };
    let listed = match places(&req.body) {
        Ok(l) => l,
        Err(e) => return Some(Response::error(400, &e)),
    };
    serve(out, tenants, streams, listed);
    None
}

/// A stream joining: put in the registry, then on the feeds of the tenants
/// open now where its client stands, then the tenants to look at read. In
/// this order a tenant made meanwhile is among them or told of: read first,
/// one made between the reading and the joining was neither, and waited
/// for the next [`RESCAN`]. `names` reads the directory.
fn begin(
    tenants: &Tenants,
    streams: &Arc<NodeStreams>,
    listed: HashMap<String, Followed>,
    names: impl FnOnce() -> Vec<String>,
) -> (Arc<Listener>, BTreeSet<String>) {
    let mut all: BTreeSet<String> = listed.keys().cloned().collect();
    let l = streams.join(listed);
    // The tenants open now took their feeds before this stream joined: it
    // is put on each where its client stands, as on one opening from here
    // on, so none closes before it is sent what it writes meanwhile -- left
    // until the stream came to it, after another tenant's image, such a
    // tenant closed with writes unsent, and they came as an image. Behind:
    // its first pass is to come, and no write waits for it until it caught
    // up there.
    for t in tenants.open_now() {
        let (Some(f), Some(feed)) = (l.followed(t.name()), t.repl.as_ref().and_then(|r| r.feed()))
        else {
            continue;
        };
        let g = l.read(&t.db);
        if !f.image
            && g.history().continues(f.history, f.cursor, g.change_seq())
            && feed.serves(f.cursor)
        {
            feed.sending_as(l.key, f.cursor, Some(&l.held), true);
            l.feed(t.name(), feed, feed.epoch());
        }
    }
    all.extend(names());
    (l, all)
}

fn serve(
    out: &mut TcpStream,
    tenants: &Tenants,
    streams: &Arc<NodeStreams>,
    listed: HashMap<String, Followed>,
) {
    let _ = out.set_write_timeout(Some(SILENCE));
    // Written without blocking, so a client reading slower than the stream
    // writes is seen as it happens, and the stream is held up for it
    // ([`Out`]).
    let _ = out.set_nonblocking(true);
    let mut head = String::from(
        "HTTP/1.1 200 OK\r\nContent-Type: application/x-fenec-replication\r\n\
         Cache-Control: no-cache, no-transform\r\nConnection: close\r\n\
         X-Accel-Buffering: no\r\n",
    );
    crate::request_id::with(|id| {
        if !id.is_empty() {
            head.push_str(crate::request_id::HEADER);
            head.push_str(": ");
            head.push_str(id);
            head.push_str("\r\n");
        }
    });
    crate::audit::push_verified(&mut head);
    head.push_str("\r\n");

    let (l, names) = begin(tenants, streams, listed, || tenants.names());
    let _leave = Leave(streams, &l);
    let peer = out.peer_addr().map(|a| a.to_string()).unwrap_or_default();
    let mut s = Session {
        tenants,
        l: &l,
        w: BufWriter::with_capacity(MESSAGE + 64, Out { s: &*out, l: &l }),
        greeted: HashSet::new(),
        lineages: HashMap::new(),
        hello_at: HashMap::new(),
        retry: BTreeSet::new(),
        pending: HashMap::new(),
        quiet: Instant::now(),
        peer: &peer,
        peeked: 0,
        looked: 0,
        images: 0,
    };
    let ended = s.run(&head, names);
    drop(s);
    crate::log!(
        "node stream from {peer} ended{}",
        ended.err().map_or(String::new(), |e| format!(": {e}"))
    );
}

/// The stream's socket, written without blocking: where the client reads
/// slower than the stream writes -- its disk's fsyncs, say -- the write
/// would block, and the stream is held up while it does.
struct Out<'a> {
    s: &'a TcpStream,
    l: &'a Listener,
}

impl Write for Out<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match (&*self.s).write(buf) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                let _held = self.l.hold();
                self.s.set_nonblocking(false)?;
                let r = (&*self.s).write(buf);
                self.s.set_nonblocking(true)?;
                r
            }
            r => r,
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// An image taken and not yet sent: its tenant's disk does not hold all of
/// it yet.
struct Pending {
    image: Vec<u8>,
    seq: u64,
    history: History,
    feed: Arc<Feed>,
    epoch: u64,
}

/// One stream, as the node sends it.
struct Session<'a> {
    tenants: &'a Tenants,
    l: &'a Arc<Listener>,
    w: BufWriter<Out<'a>>,
    /// The tenants said hello to on this stream.
    greeted: HashSet<String>,
    /// The history last sent each.
    lineages: HashMap<String, Vec<(u64, u64)>>,
    /// When each was last said hello to, by the node's clock: a delete
    /// after it is the client's copy gone, though the tenant is back.
    hello_at: HashMap<String, u64>,
    /// Tenants to look at again: busy opening or closing, released from
    /// under the stream, or an image waiting for its disk.
    retry: BTreeSet<String>,
    pending: HashMap<String, Pending>,
    /// When the stream last sent anything.
    quiet: Instant,
    peer: &'a str,
    /// Tenants read from their files, opened to be looked at, and images
    /// sent: what its first pass cost, which the log says.
    peeked: usize,
    looked: usize,
    images: usize,
}

impl Drop for Session<'_> {
    /// The feeds of images not sent are no feeds the registry knows of.
    fn drop(&mut self) {
        for (_, p) in self.pending.drain() {
            p.feed.stopped(self.l.key);
        }
    }
}

impl Session<'_> {
    fn run(&mut self, head: &str, names: BTreeSet<String>) -> io::Result<()> {
        self.w.write_all(head.as_bytes())?;
        message(&mut self.w, b'N', &[&[VERSION]])?;
        self.w.flush()?;
        let t0 = Instant::now();
        for name in &names {
            self.pump(name)?;
        }
        self.w.flush()?;
        crate::log!(
            "node stream from {}: {} tenants said hello to in {:.1?}: {} read from their \
             files, {} opened to be looked at, {} images",
            self.peer,
            names.len(),
            t0.elapsed(),
            self.peeked,
            self.looked,
            self.images
        );
        let mut scanned = Instant::now();
        loop {
            let wait = if self.retry.is_empty() { ALIVE } else { RETRY };
            let mut due = self.l.take(wait);
            due.append(&mut self.retry);
            for name in &due {
                self.pump(name)?;
            }
            if scanned.elapsed() >= RESCAN {
                self.rescan()?;
                scanned = Instant::now();
            }
            if self.quiet.elapsed() >= ALIVE {
                message(&mut self.w, b'K', &[&u64s(&[now_ms()])])?;
                self.quiet = Instant::now();
            }
            self.w.flush()?;
        }
    }

    /// The directory read again: a tenant's file that came or went with
    /// nothing telling the stream.
    fn rescan(&mut self) -> io::Result<()> {
        let on_disk: BTreeSet<String> = self.tenants.names().into_iter().collect();
        let missing = self.greeted.iter().filter(|n| !on_disk.contains(*n));
        let new = on_disk.iter().filter(|n| !self.greeted.contains(*n));
        let due: Vec<String> = missing.chain(new).cloned().collect();
        for name in &due {
            self.pump(name)?;
        }
        Ok(())
    }

    fn send(&mut self, tag: u8, name: &str, parts: &[&[u8]]) -> io::Result<()> {
        let len = [name.len() as u8];
        let mut all: Vec<&[u8]> = Vec::with_capacity(parts.len() + 2);
        all.push(&len);
        all.push(name.as_bytes());
        all.extend_from_slice(parts);
        message(&mut self.w, tag, &all)?;
        self.quiet = Instant::now();
        Ok(())
    }

    /// Sends what is new of `name`: its hello where it has had none, its
    /// writes where it has, that it is gone where its file is.
    fn pump(&mut self, name: &str) -> io::Result<()> {
        if !self.tenants.on_disk(name) {
            return self.gone(name);
        }
        if self.pending.contains_key(name) {
            return self.send_pending(name);
        }
        if !self.greeted.contains(name) {
            return self.resolve(name);
        }
        let fed = {
            let st = self.l.state();
            st.feeds
                .get(name)
                .map(|(f, e)| (Arc::clone(f), *e))
                .zip(st.following.get(name).map(|f| f.cursor))
        };
        let Some(((feed, epoch), mut cursor)) = fed else {
            return self.unfed(name);
        };
        loop {
            match feed.next(cursor, epoch, MESSAGE) {
                Next::Records(first, last, times, records) => {
                    // The history first: the writes after a promotion are
                    // on the history it began, which the feed holds by now.
                    self.lineage(name, &feed)?;
                    let n = times.len() as u64;
                    self.send(b'W', name, &[&u64s(&[first, n]), &u64s(&times), &records])?;
                    self.w.flush()?;
                    cursor = last;
                    // Where the client stands, before the feed is told: told
                    // first, the tenant could close at its new place while
                    // the stream still held the old one, and open again
                    // there as if the client stood elsewhere.
                    self.l.moved(name, cursor);
                    feed.sent(self.l.key, cursor, false);
                }
                Next::Nothing => {
                    feed.sent(self.l.key, cursor, true);
                    return self.lineage(name, &feed);
                }
                Next::Behind | Next::Moved => {
                    // The feed no longer leads from where the client stands:
                    // an image, as a replica is sent.
                    self.l.unfeed(name, &feed);
                    self.greeted.remove(name);
                    return self.resolve(name);
                }
                Next::Gone => {
                    // Released -- being deleted, made to follow, moved --
                    // and looked at again until it is gone or back: its
                    // notices may have come while the stream was busy, and
                    // a tenant made again under the name, writing nothing,
                    // would give none more.
                    self.l.unfeed(name, &feed);
                    self.retry.insert(name.into());
                    return Ok(());
                }
            }
        }
    }

    /// A tenant said hello to with no feed to send from: closed, which is
    /// read again, since a holder of its database may have written it after
    /// it closed, or opened where the client did not stand, which is judged
    /// again.
    fn unfed(&mut self, name: &str) -> io::Result<()> {
        self.left(name)?;
        let followed = self.l.followed(name);
        match self.tenants.seen(name) {
            Seen::Open(t) => self.judge(name, &t, followed),
            Seen::Busy => {
                self.retry.insert(name.into());
                Ok(())
            }
            Seen::Missing => self.gone(name),
            Seen::Closed(_) => self.closed(name, followed, None, false),
        }
    }

    /// Says hello to `name`: where the client goes on from, or an image.
    fn resolve(&mut self, name: &str) -> io::Result<()> {
        self.left(name)?;
        let followed = self.l.followed(name);
        match self.tenants.seen(name) {
            Seen::Missing => self.gone(name),
            Seen::Busy => {
                self.retry.insert(name.into());
                Ok(())
            }
            Seen::Open(t) => self.judge(name, &t, followed),
            Seen::Closed(place) => self.closed(name, followed, place, true),
        }
    }

    /// A closed tenant: where it stands -- `place`, as it closed in this
    /// process, or read from its file rather than opened, whose fsync is
    /// an image's to pay -- and where the client stands there, nothing to
    /// send (a hello where `greet`); opened otherwise, for an image or to
    /// go on from its feed.
    fn closed(
        &mut self,
        name: &str,
        followed: Option<Followed>,
        place: Option<(u64, History)>,
        greet: bool,
    ) -> io::Result<()> {
        let l = self.l;
        let place = match (place, followed) {
            (Some(p), _) => Some(p),
            (None, Some(f)) if !f.image => {
                let _held = l.hold();
                self.peeked += 1;
                match self.tenants.peek(name) {
                    Ok(p) => p,
                    Err(Refused(404, _)) => return self.gone(name),
                    Err(_) => None,
                }
            }
            (None, _) => None,
        };
        if let Some((seq, history)) = place.filter(|(seq, h)| stands_at(followed, *seq, h)) {
            if greet {
                self.hello(name, None, seq, seq, &history)?;
                self.l.follow(name, seq, history.current());
                self.greeted.insert(name.into());
                self.lineages.insert(name.into(), history.lineage);
            }
            return Ok(());
        }
        let tenants = self.tenants;
        self.looked += 1;
        let _held = l.hold();
        match tenants.look(name, |t| self.judge(name, t, followed)) {
            Ok(r) => r,
            Err(Refused(404, _)) => self.gone(name),
            Err(Refused(503, _)) => Ok(()),
            Err(Refused(_, why)) => {
                crate::log!("node stream: tenant `{name}` could not be looked at: {why}");
                Ok(())
            }
        }
    }

    /// Where the client goes on in an open tenant, judged against the
    /// database and the feed at one moment, as a replica's stream judges:
    /// from its place where the feed still leads from there, from an image
    /// otherwise.
    fn judge(&mut self, name: &str, t: &Arc<Tenant>, followed: Option<Followed>) -> io::Result<()> {
        let Some(feed) = t.repl.as_ref().and_then(|r| r.feed()).cloned() else {
            return Ok(());
        };
        let l = self.l;
        let image = {
            let g = l.read(&t.db);
            let now = g.change_seq();
            let epoch = feed.epoch();
            match followed {
                Some(f)
                    if !f.image
                        && g.history().continues(f.history, f.cursor, now)
                        && feed.serves(f.cursor) =>
                {
                    // Put on the feed while the read lock keeps writes out:
                    // the tenant holds every write after the place open
                    // until it is sent.
                    feed.sending_as(l.key, f.cursor, Some(&l.held), false);
                    l.feed(name, &feed, epoch);
                    let history = g.history().clone();
                    drop(g);
                    self.hello(name, None, f.cursor, feed.durable(), &history)?;
                    l.follow(name, f.cursor, history.current());
                    self.greeted.insert(name.into());
                    self.lineages.insert(name.into(), history.lineage);
                    // What came after the hello goes in the next round, not
                    // from here: a tenant that fell behind again would be
                    // judged again inside it.
                    l.mark(name);
                    return Ok(());
                }
                _ => {
                    // An image, its snapshot as long as the tenant is large:
                    // held up for it. On the feed behind it, so the tenant
                    // keeps the writes after it until they are sent and no
                    // write waits for the stream until it has caught up.
                    let _held = l.hold();
                    let image = g.snapshot();
                    feed.sending_as(l.key, now, Some(&l.held), true);
                    Pending {
                        image,
                        seq: now,
                        history: g.history().clone(),
                        feed: Arc::clone(&feed),
                        epoch,
                    }
                }
            }
        };
        self.greeted.remove(name);
        // Among the stream's feeds while it waits, where a close takes it
        // off (`NodeStreams::detach`) and an open puts another in its place
        // (`attach`): which `send_pending` asks before it sends.
        l.feed(name, &feed, image.epoch);
        if let Some(old) = self.pending.insert(name.into(), image) {
            if !Arc::ptr_eq(&old.feed, &feed) {
                old.feed.stopped(l.key);
            }
        }
        self.send_pending(name)
    }

    /// Sends an image taken, once its tenant's disk holds it; looked at
    /// again otherwise, the stream going on meanwhile -- waited for here, up
    /// to 30 s, it held every other tenant's writes up with it.
    fn send_pending(&mut self, name: &str) -> io::Result<()> {
        let l = self.l;
        let Some(p) = self.pending.get(name) else {
            return Ok(());
        };
        let live = l
            .state()
            .feeds
            .get(name)
            .is_some_and(|(f, _)| Arc::ptr_eq(f, &p.feed));
        if !live || p.feed.epoch() != p.epoch || p.feed.is_closed() {
            // The tenant closed -- its feed went with it, and an open since
            // has one of its own -- or the feed started over or is going:
            // taken again from the tenant as it is now. Sent, the image put
            // the dead feed back among the stream's, which read nothing
            // from it again: the archive stood still until the archiver
            // connected again.
            let p = self.pending.remove(name).expect("looked at above");
            l.unfeed(name, &p.feed);
            p.feed.stopped(l.key);
            return self.resolve(name);
        }
        if p.feed.durable() < p.seq {
            self.retry.insert(name.into());
            return Ok(());
        }
        let p = self.pending.remove(name).expect("looked at above");
        {
            let _held = l.hold();
            self.hello(name, Some(&p.image), p.seq, p.feed.durable(), &p.history)?;
        }
        self.images += 1;
        // Its feed is among the stream's already, or a close took it off
        // as the image went: put back, it would be dead.
        l.follow(name, p.seq, p.history.current());
        self.greeted.insert(name.into());
        self.lineages.insert(name.into(), p.history.lineage);
        l.mark(name);
        Ok(())
    }

    fn hello(
        &mut self,
        name: &str,
        image: Option<&[u8]>,
        seq: u64,
        durable: u64,
        history: &History,
    ) -> io::Result<()> {
        let now = now_ms();
        let mut hello = vec![VERSION, image.is_some() as u8];
        hello.extend_from_slice(&u64s(&[seq, durable, now]));
        hello.extend_from_slice(&history.encode());
        self.send(b'H', name, &[&hello])?;
        self.hello_at.insert(name.into(), now);
        if let Some(image) = image {
            self.send(b'I', name, &[image])?;
        }
        self.w.flush()
    }

    /// The history the feed says the writes are on, sent when it moved.
    fn lineage(&mut self, name: &str, feed: &Feed) -> io::Result<()> {
        let now = feed.lineage();
        if now.is_empty() || self.lineages.get(name) == Some(&now) {
            return Ok(());
        }
        let h = History {
            lineage: now.clone(),
            following: false,
        };
        self.send(b'L', name, &[&h.encode()])?;
        self.w.flush()?;
        self.l.rehistory(name, h.current());
        self.lineages.insert(name.into(), now);
        Ok(())
    }

    /// `name` is not on the node: the client is told, where it followed it,
    /// with when it went -- the node's time it was deleted, where this
    /// process deleted it since the stream said hello to it, or now. A
    /// delete before that hello was another copy's: one gone since by other
    /// means -- its file taken away by hand -- given that time, read as
    /// away from before its hello.
    fn gone(&mut self, name: &str) -> io::Result<()> {
        let at = match (self.tenants.departed(name), self.hello_at.get(name)) {
            (Some(at), Some(&said)) if at >= said => at,
            (Some(at), None) => at,
            _ => now_ms(),
        };
        self.gone_at(name, at)
    }

    fn gone_at(&mut self, name: &str, at: u64) -> io::Result<()> {
        if let Some(p) = self.pending.remove(name) {
            self.l.unfeed(name, &p.feed);
            p.feed.stopped(self.l.key);
        }
        let followed = self.l.unfollow(name);
        let greeted = self.greeted.remove(name);
        self.lineages.remove(name);
        self.hello_at.remove(name);
        if followed || greeted {
            self.send(b'G', name, &[&at.to_le_bytes()])?;
            self.w.flush()?;
        }
        Ok(())
    }

    /// The client's copy of `name` is gone where the node deleted the
    /// tenant -- or moved it off -- after it said hello to it, though a
    /// tenant of the name is there now: made again, or moved back, while
    /// the stream was busy, and noticed only as a new copy, which left the
    /// old line no word that it went.
    fn left(&mut self, name: &str) -> io::Result<()> {
        let Some(&greeted_at) = self.hello_at.get(name) else {
            return Ok(());
        };
        match self.tenants.departed(name) {
            Some(at) if at >= greeted_at => self.gone_at(name, at),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places_read_a_line_a_tenant_and_refuse_anything_else() {
        let p = places(b"acme 41 00000000000000ff 0\n\nbeta 0 0 1\n").unwrap();
        assert_eq!(p.len(), 2);
        let a = p["acme"];
        assert_eq!((a.cursor, a.history, a.image), (41, 255, false));
        assert!(p["beta"].image);
        for bad in [
            &b"acme 41 ff"[..],
            b"acme x ff 0",
            b"acme 1 zz 0",
            b"acme 1 ff 2",
            b"Acme 1 ff 0",
            b"../x 1 ff 0",
            b"acme 1 ff 0 extra",
        ] {
            assert!(places(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
    }

    fn record() -> Vec<u8> {
        let mut r = vec![3u8, 1];
        fenec_core::codec::put_uvarint(&mut r, 4);
        r.extend([1, 1, 1, 0xcd]);
        r
    }

    fn standing(cursor: u64, history: u64) -> HashMap<String, Followed> {
        let mut following = HashMap::new();
        following.insert(
            "t".to_string(),
            Followed {
                cursor,
                history,
                image: false,
            },
        );
        following
    }

    /// A stream on a tenant's feed holds the tenant open until it has sent
    /// what the disk holds, and a second put of the same stream on the feed
    /// moves it rather than leaving an entry nobody moves on.
    #[test]
    fn a_feed_waits_for_its_streams_before_its_tenant_closes() {
        let feed = Feed::new(1 << 20);
        feed.start(10);
        let streams = NodeStreams::new();
        let l = streams.join(standing(10, 7));
        let history = History {
            lineage: vec![(7, 0)],
            following: false,
        };
        streams.attach("t", &feed, 10, &history);
        streams.attach("t", &feed, 10, &history);
        assert!(!feed.unsent());
        // A write on disk the stream has not sent.
        feed.push_durable(11, &record());
        assert!(feed.unsent());
        // Told: the stream has it among those to look at.
        assert!(l.take(Duration::ZERO).contains("t"));
        feed.sent(l.key, 11, true);
        assert!(!feed.unsent());
        // Gone from the registry, it is off the feed.
        drop(Leave(&streams, &l));
        feed.push_durable(12, &record());
        assert!(!feed.unsent());
    }

    #[test]
    fn a_stream_standing_elsewhere_is_told_to_look_rather_than_put_on_the_feed() {
        let feed = Feed::new(1 << 20);
        feed.start(20);
        let streams = NodeStreams::new();
        let l = streams.join(standing(15, 7));
        let history = History {
            lineage: vec![(7, 0)],
            following: false,
        };
        streams.attach("t", &feed, 20, &history);
        assert!(l.state().feeds.is_empty());
        assert!(l.take(Duration::ZERO).contains("t"));
        // Nor on another history's place, though the number matches.
        l.follow("t", 20, 9);
        streams.attach("t", &feed, 20, &history);
        assert!(l.state().feeds.is_empty());
    }

    /// A write waits for the stream while it keeps up, and for none while
    /// it is held up -- waking one that waits already -- though the tenant
    /// still keeps the write for it.
    #[test]
    fn a_write_waits_for_no_stream_held_up() {
        let feed = Feed::new(1 << 20);
        feed.start(10);
        let streams = NodeStreams::new();
        let l = streams.join(standing(10, 7));
        let history = History {
            lineage: vec![(7, 0)],
            following: false,
        };
        streams.attach("t", &feed, 10, &history);
        feed.push_durable(11, &record());
        // Keeping up: the wait runs out.
        let t0 = Instant::now();
        feed.wait_sent(11, Duration::from_millis(200));
        assert!(t0.elapsed() >= Duration::from_millis(200));
        feed.sent(l.key, 11, true);
        feed.push_durable(12, &record());
        // A wait under way -- or begun after -- is let go once the stream is
        // held up, though its timeout is a minute off.
        let waiting = {
            let feed = Arc::clone(&feed);
            std::thread::spawn(move || {
                let t0 = Instant::now();
                feed.wait_sent(12, Duration::from_secs(60));
                t0.elapsed()
            })
        };
        let held = l.hold();
        assert!(waiting.join().unwrap() < Duration::from_secs(30));
        let t0 = Instant::now();
        feed.wait_sent(12, Duration::from_secs(60));
        assert!(t0.elapsed() < Duration::from_secs(30));
        assert!(feed.unsent(), "held up, the tenant still keeps the write");
        drop(held);
        assert!(!l.held.load(Ordering::SeqCst));
    }

    /// A tenant made as a stream joins -- after the directory is read --
    /// is among those it looks at or told of: read before the joining, it
    /// was neither, and waited for the next rescan.
    #[test]
    fn a_tenant_made_as_a_stream_joins_is_looked_at() {
        let dir = std::env::temp_dir().join(format!(
            "fenec-node-stream-begin-{}-{:?}",
            std::process::id(),
            Instant::now()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let tenants = Tenants::new(&dir)
            .unwrap()
            .with_replication(crate::tenants::Replicated {
                token: "r".into(),
                buffer: 1 << 20,
                upstream: None,
                sync_on_write: true,
            });
        let streams = Arc::clone(tenants.node_streams().unwrap());
        let (l, names) = begin(&tenants, &streams, HashMap::new(), || {
            let read = tenants.names();
            drop(tenants.create("late").unwrap());
            read
        });
        assert!(
            names.contains("late") || l.take(Duration::ZERO).contains("late"),
            "a tenant made as the stream joined went unseen"
        );
        drop(Leave(&streams, &l));
        drop(tenants);
        let _ = std::fs::remove_dir_all(dir);
    }
}
