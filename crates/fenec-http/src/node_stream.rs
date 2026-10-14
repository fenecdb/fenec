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
//! there as the tenant opens ([`NodeStreams::attach`]), where it stands
//! where the file does -- and [`crate::tenants::Tenants`] closes a tenant
//! only when no sender has a write left to send ([`Feed::unsent`]); under
//! `--sync always` a durable write waits for the stream as for a replica.
//! A tenant that opens again starts a new feed where the old one ended, and
//! the stream goes on from it with no image.
//!
//! **Which tenants have news is told, not polled.** A tenant's feed tells
//! the node's streams as its writes reach the disk, it starts over or it
//! closes (`Feed::tell_with`), a tenant made, installed, deleted, promoted
//! or made to follow tells them too, and a stream looks only at the tenants
//! it was told of -- and at the directory every [`RESCAN`], for a file put
//! there by hand. With no stream connected a tenant's sync pays a load.
//!
//! **Where a closed tenant stands is known without opening it** where it
//! closed in this process ([`crate::tenants::Seen::Closed`]); otherwise --
//! the first stream after the node starts -- the tenant is opened to look,
//! and closed again.
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
//! G  gone     [name]: no longer on this node, deleted or moved to another
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
use crate::replication::{
    message, u64s, Feed, Next, ALIVE, DURABLE_WAIT, MESSAGE, SILENCE, VERSION,
};
use crate::tenants::{check_name, Refused, Seen, Tenant, Tenants};
use fenec_core::history::History;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{self, BufWriter, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How often a stream reads the directory again, for a tenant's file that
/// came or went with nothing telling it: an operator's copy, a restore.
/// Every change made through the node is told at once.
pub const RESCAN: Duration = Duration::from_secs(10);

/// How soon a tenant that was busy opening or closing as it was looked at
/// is looked at again.
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
    /// write, so the write waits for the stream and the tenant stays open
    /// until it is sent. A stream that stands elsewhere is told to look.
    pub(crate) fn attach(
        self: &Arc<Self>,
        name: &str,
        feed: &Arc<Feed>,
        seq: u64,
        history: &History,
    ) {
        let weak = Arc::downgrade(self);
        let tenant = name.to_string();
        feed.tell_with(move || {
            if let Some(streams) = weak.upgrade() {
                streams.touch(&tenant);
            }
        });
        if self.count.load(Ordering::Relaxed) == 0 {
            return;
        }
        for l in lock(&self.listeners).iter() {
            let mut st = lock(&l.state);
            let here = st.following.get(name).is_some_and(|f| {
                !f.image && f.cursor == seq && history.continues(f.history, seq, seq)
            });
            if here {
                feed.sending(l.key, seq);
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

impl Listener {
    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
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

fn serve(
    out: &mut TcpStream,
    tenants: &Tenants,
    streams: &Arc<NodeStreams>,
    listed: HashMap<String, Followed>,
) {
    let _ = out.set_write_timeout(Some(SILENCE));
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

    let names: BTreeSet<String> = tenants
        .names()
        .into_iter()
        .chain(listed.keys().cloned())
        .collect();
    let l = streams.join(listed);
    let _leave = Leave(streams, &l);
    // The tenants open now took their feeds before this stream joined: it
    // is put on each where its client stands, as on one opening from here
    // on, so none closes before it is sent what it writes meanwhile. Left
    // until the stream came to it -- after another tenant's image, say --
    // such a tenant closed with writes unsent, and they came as an image.
    for t in tenants.open_now() {
        let (Some(f), Some(feed)) = (l.followed(t.name()), t.repl.as_ref().and_then(|r| r.feed()))
        else {
            continue;
        };
        let g = crate::held::read(&t.db);
        if !f.image
            && g.history().continues(f.history, f.cursor, g.change_seq())
            && feed.serves(f.cursor)
        {
            feed.sending(l.key, f.cursor);
            l.feed(t.name(), feed, feed.epoch());
        }
    }
    let peer = out.peer_addr().map(|a| a.to_string()).unwrap_or_default();
    let mut s = Session {
        tenants,
        l: &l,
        w: BufWriter::with_capacity(MESSAGE + 64, &*out),
        greeted: HashSet::new(),
        lineages: HashMap::new(),
        retry: BTreeSet::new(),
        quiet: Instant::now(),
        peer: &peer,
        peeked: 0,
        looked: 0,
        images: 0,
    };
    let ended = s.run(&head, names);
    crate::log!(
        "node stream from {peer} ended{}",
        ended.err().map_or(String::new(), |e| format!(": {e}"))
    );
}

/// One stream, as the node sends it.
struct Session<'a> {
    tenants: &'a Tenants,
    l: &'a Arc<Listener>,
    w: BufWriter<&'a TcpStream>,
    /// The tenants said hello to on this stream.
    greeted: HashSet<String>,
    /// The history last sent each.
    lineages: HashMap<String, Vec<(u64, u64)>>,
    /// Tenants busy opening or closing when looked at, looked at again.
    retry: BTreeSet<String>,
    /// When the stream last sent anything.
    quiet: Instant,
    peer: &'a str,
    /// Tenants read from their files, opened to be looked at, and images
    /// sent: what its first pass cost, which the log says.
    peeked: usize,
    looked: usize,
    images: usize,
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
                    feed.sent(self.l.key, cursor, false);
                    self.l.moved(name, cursor);
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
                    // and told of again once it is done.
                    self.l.unfeed(name, &feed);
                    return Ok(());
                }
            }
        }
    }

    /// A tenant said hello to with no feed to send from: closed, which
    /// sends nothing, or opened where the client did not stand, which is
    /// judged again.
    fn unfed(&mut self, name: &str) -> io::Result<()> {
        match self.tenants.seen(name) {
            Seen::Open(t) => {
                let followed = self.l.followed(name);
                self.judge(name, &t, followed)
            }
            Seen::Busy => {
                self.retry.insert(name.into());
                Ok(())
            }
            Seen::Closed(_) | Seen::Missing => Ok(()),
        }
    }

    /// Says hello to `name`: where the client goes on from, or an image.
    fn resolve(&mut self, name: &str) -> io::Result<()> {
        let followed = self.l.followed(name);
        match self.tenants.seen(name) {
            Seen::Missing => self.gone(name),
            Seen::Busy => {
                self.retry.insert(name.into());
                Ok(())
            }
            Seen::Open(t) => self.judge(name, &t, followed),
            Seen::Closed(place) => {
                // Where it stands: as it closed in this process, or read
                // from its file where the client may stand there -- the
                // first stream after the node started -- rather than
                // opened, whose fsync is the image's to pay.
                let place = match (place, followed) {
                    (Some(p), _) => Some(p),
                    (None, Some(f)) if !f.image => {
                        self.peeked += 1;
                        self.tenants.peek(name).ok().flatten()
                    }
                    (None, _) => None,
                };
                if let Some((seq, history)) = place.filter(|(seq, history)| {
                    followed.is_some_and(|f| {
                        !f.image && f.cursor == *seq && history.continues(f.history, *seq, *seq)
                    })
                }) {
                    // Closed where the client stands: nothing to send, and
                    // nothing opened to say so.
                    self.hello(name, None, seq, seq, &history)?;
                    self.l.follow(name, seq, history.current());
                    self.greeted.insert(name.into());
                    self.lineages.insert(name.into(), history.lineage);
                    return Ok(());
                }
                let tenants = self.tenants;
                self.looked += 1;
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
        let (seq, history, epoch, image) = {
            let g = crate::held::read(&t.db);
            let now = g.change_seq();
            let epoch = feed.epoch();
            match followed {
                Some(f)
                    if !f.image
                        && g.history().continues(f.history, f.cursor, now)
                        && feed.serves(f.cursor) =>
                {
                    // Put on the feed while the read lock keeps writes out:
                    // every write after the place waits for this stream, and
                    // holds the tenant open until it is sent.
                    feed.sending(self.l.key, f.cursor);
                    self.l.feed(name, &feed, epoch);
                    (f.cursor, g.history().clone(), epoch, None)
                }
                _ => (now, g.history().clone(), epoch, Some(g.snapshot())),
            }
        };
        if image.is_some() {
            // An image goes out once the tenant's disk holds all of it.
            let deadline = Instant::now() + DURABLE_WAIT;
            while feed.durable() < seq {
                if Instant::now() > deadline {
                    self.retry.insert(name.into());
                    return Ok(());
                }
                feed.wait(seq.saturating_sub(1), epoch, Duration::from_millis(100));
            }
        }
        self.hello(name, image.as_deref(), seq, feed.durable(), &history)?;
        if image.is_some() {
            self.images += 1;
            feed.sending(self.l.key, seq);
            self.l.feed(name, &feed, epoch);
        }
        self.l.follow(name, seq, history.current());
        self.greeted.insert(name.into());
        self.lineages.insert(name.into(), history.lineage);
        // What came after the hello goes in the next round, not from here:
        // a tenant that fell behind again would be judged again inside it.
        self.l.mark(name);
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
        let mut hello = vec![VERSION, image.is_some() as u8];
        hello.extend_from_slice(&u64s(&[seq, durable, now_ms()]));
        hello.extend_from_slice(&history.encode());
        self.send(b'H', name, &[&hello])?;
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

    /// `name` is not on the node: the client is told, where it followed it.
    fn gone(&mut self, name: &str) -> io::Result<()> {
        let followed = self.l.unfollow(name);
        let greeted = self.greeted.remove(name);
        self.lineages.remove(name);
        if followed || greeted {
            self.send(b'G', name, &[])?;
            self.w.flush()?;
        }
        Ok(())
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

    /// A stream on a tenant's feed holds the tenant open until it has sent
    /// what the disk holds, and a second put of the same stream on the feed
    /// moves it rather than leaving an entry nobody moves on.
    #[test]
    fn a_feed_waits_for_its_streams_before_its_tenant_closes() {
        let feed = Feed::new(1 << 20);
        feed.start(10);
        let streams = NodeStreams::new();
        let mut following = HashMap::new();
        following.insert(
            "t".to_string(),
            Followed {
                cursor: 10,
                history: 7,
                image: false,
            },
        );
        let l = streams.join(following);
        let history = History {
            lineage: vec![(7, 0)],
            following: false,
        };
        streams.attach("t", &feed, 10, &history);
        streams.attach("t", &feed, 10, &history);
        assert!(!feed.unsent());
        // A write on disk the stream has not sent.
        let record = {
            let mut r = vec![3u8, 1];
            fenec_core::codec::put_uvarint(&mut r, 4);
            r.extend([1, 1, 1, 0xcd]);
            r
        };
        feed.push_durable(11, &record);
        assert!(feed.unsent());
        // Told: the stream has it among those to look at.
        assert!(l.take(Duration::ZERO).contains("t"));
        feed.sent(l.key, 11, true);
        assert!(!feed.unsent());
        // Gone from the registry, it is off the feed.
        drop(Leave(&streams, &l));
        feed.push_durable(12, &record);
        assert!(!feed.unsent());
    }

    #[test]
    fn a_stream_standing_elsewhere_is_told_to_look_rather_than_put_on_the_feed() {
        let feed = Feed::new(1 << 20);
        feed.start(20);
        let streams = NodeStreams::new();
        let mut following = HashMap::new();
        following.insert(
            "t".to_string(),
            Followed {
                cursor: 15,
                history: 7,
                image: false,
            },
        );
        let l = streams.join(following);
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
}
