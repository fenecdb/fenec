//! Log shipping: a primary streams the writes it appends to its file, and a
//! replica applies them to its own.
//!
//! **What is sent.** A primary's file is its log: an image, then each write
//! appended as a record. A replica is sent those records, whole and in
//! order, and applies each the way the write path made it
//! ([`Database::apply`]); its own file ends up holding the same records, so
//! it reopens where it stopped and asks for the rest. A replica too far
//! behind for the primary's buffer -- or on a history the primary's does
//! not continue ([`History`]) -- is sent an image instead.
//!
//! **Only what is on the primary's disk.** A record reaches the feed when it
//! is appended and leaves it once an fsync has covered it. A primary that
//! crashes therefore comes back holding every write any replica was sent,
//! and no replica ever has to be told to forget one. The price is latency:
//! under `--sync 250` a write reaches the replicas up to 250 ms later, under
//! `--sync always` right after its own fsync. `--sync off` makes nothing
//! durable before shutdown, so a server with replicas refuses it.
//!
//! **Positions are change numbers.** Every write moves the change counter by
//! one, on the primary as on its replicas: a replica's position is its own
//! counter, a subscriber's cursor means the same change on either, and a
//! replica of a replica is fed exactly as its primary was.
//!
//! ## Wire format
//!
//! `GET /_replication?since=<seq>&history=<id, hex>` with
//! `Authorization: Bearer <replication token>` answers with a stream of
//! messages, `[tag: u8][length: u64 LE][payload]`:
//!
//! ```text
//! H  hello   [version: 1][image: u8][seq u64][durable u64][time_ms u64][history]
//! I  image   the database as of the hello's seq (when `image` is 1)
//! W  writes  [first seq u64][n u64][n x time_ms u64][n records]
//! K  alive   [seq u64][durable u64][time_ms u64]
//! E  end     why, in UTF-8; the stream closes after it
//! ```
//!
//! A write's time is when the primary appended it, which is what a restore
//! to a moment goes by (see [`crate::archive`]).
//!
//! `&image=1` asks for an image whatever the positions say: a replica sends
//! it after a record it could not apply.

use crate::constant_eq;
use crate::http::{Request, Response};
use fenec_core::engine::MAGIC;
use fenec_core::fs::FileSink;
use fenec_core::prelude::*;
use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Default size of a primary's feed. A replica that falls further behind is
/// sent an image. 64 MiB holds ~20 000 writes of a 768-dimension document,
/// about a minute of a sustained 300 writes/s.
pub const DEFAULT_BUFFER: usize = 64 << 20;

/// The feed is kept and dropped a chunk at a time: this much, or an eighth
/// of a smaller buffer, so that one still drops its oldest writes first.
const CHUNK: usize = 1 << 20;

/// The records one `W` message carries, at most; a larger record goes alone.
/// It is also what a replica applies under one hold of its write lock.
const MESSAGE: usize = 1 << 20;

/// A stream with nothing to send says so this often: the replica learns the
/// primary is there, and how far it has got.
const ALIVE: Duration = Duration::from_secs(2);

/// A replica that hears nothing for this long takes the primary for gone and
/// connects again.
const SILENCE: Duration = Duration::from_secs(10);

/// An image is sent once the primary's disk holds it; a primary whose disk
/// does not get there in this long is not sending it.
const DURABLE_WAIT: Duration = Duration::from_secs(30);

const VERSION: u8 = 1;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// A history id nobody else holds: 64 bits from the operating system's
/// randomness (`RandomState` is seeded from it), never 0 -- the root's.
pub fn fresh_id() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    h.write_u32(std::process::id());
    h.finish().max(1)
}

// --------------------------------------------------------------------- feed

/// The writes a primary appended, kept for its replicas: the last
/// `capacity` bytes of them, and how far its disk holds them.
pub struct Feed {
    ring: Mutex<Ring>,
    cv: Condvar,
}

struct Ring {
    chunks: VecDeque<Chunk>,
    bytes: usize,
    cap: usize,
    chunk: usize,
    /// The last write appended, and the last one on disk.
    seq: u64,
    durable: u64,
    /// Moves when what the ring holds stops leading to the database -- a
    /// replica that took an image: a stream begun before it ends.
    epoch: u64,
}

/// Consecutive records, the first numbered `first`.
struct Chunk {
    first: u64,
    data: Vec<u8>,
    /// Where each record ends in `data`, and when it was appended.
    ends: Vec<usize>,
    times: Vec<u64>,
}

enum Next {
    /// The first one's number, their times, the records.
    Records(u64, Vec<u64>, Vec<u8>),
    Nothing,
    /// The records after the cursor are no longer kept.
    Behind,
    /// The feed started over.
    Moved,
}

impl Feed {
    pub fn new(capacity: usize) -> Arc<Feed> {
        Arc::new(Feed {
            ring: Mutex::new(Ring {
                chunks: VecDeque::new(),
                bytes: 0,
                cap: capacity.max(1),
                chunk: CHUNK.min(capacity / 8).max(1),
                seq: 0,
                durable: 0,
                epoch: 0,
            }),
            cv: Condvar::new(),
        })
    }

    /// Starts over at `seq`, every write up to it on disk and none kept:
    /// the database was opened there, or replaced by an image.
    pub fn start(&self, seq: u64) {
        let mut r = lock(&self.ring);
        r.chunks.clear();
        r.bytes = 0;
        r.seq = seq;
        r.durable = seq;
        r.epoch += 1;
        drop(r);
        self.cv.notify_all();
    }

    /// The last write appended.
    pub fn seq(&self) -> u64 {
        lock(&self.ring).seq
    }

    /// The last write on disk: the furthest a replica can be.
    pub fn durable(&self) -> u64 {
        lock(&self.ring).durable
    }

    fn epoch(&self) -> u64 {
        lock(&self.ring).epoch
    }

    fn push(&self, seq: u64, bytes: &[u8]) {
        let mut r = lock(&self.ring);
        if seq != r.seq + 1 {
            // Not the write after the last one: what the ring holds no
            // longer leads here. Never seen, since every write passes
            // through; kept so a mistake costs images, not wrong replicas.
            r.chunks.clear();
            r.bytes = 0;
            r.epoch += 1;
            r.durable = r.durable.min(seq - 1);
        }
        let room = match r.chunks.back() {
            Some(c) => c.data.len() + bytes.len() <= r.chunk,
            None => false,
        };
        if !room {
            r.chunks.push_back(Chunk {
                first: seq,
                data: Vec::new(),
                ends: Vec::new(),
                times: Vec::new(),
            });
        }
        let c = r.chunks.back_mut().unwrap();
        c.data.extend_from_slice(bytes);
        c.ends.push(c.data.len());
        c.times.push(now_ms());
        r.bytes += bytes.len();
        r.seq = seq;
        while r.bytes > r.cap && r.chunks.len() > 1 {
            let old = r.chunks.pop_front().unwrap();
            r.bytes -= old.data.len();
        }
    }

    fn mark_durable(&self, upto: u64) {
        let mut r = lock(&self.ring);
        let upto = upto.min(r.seq);
        if upto > r.durable {
            r.durable = upto;
            drop(r);
            self.cv.notify_all();
        }
    }

    /// Whether every write after `cursor` is still kept.
    fn serves(&self, cursor: u64) -> bool {
        let r = lock(&self.ring);
        cursor == r.seq || cursor < r.seq && r.chunks.front().is_some_and(|c| c.first <= cursor + 1)
    }

    /// The durable records after `cursor`, up to about `max` bytes.
    fn next(&self, cursor: u64, epoch: u64, max: usize) -> Next {
        let r = lock(&self.ring);
        if r.epoch != epoch {
            return Next::Moved;
        }
        if cursor >= r.durable {
            return Next::Nothing;
        }
        let first = cursor + 1;
        let Some(at) = r
            .chunks
            .iter()
            .position(|c| first >= c.first && first < c.first + c.ends.len() as u64)
        else {
            return Next::Behind;
        };
        let mut out = Vec::new();
        let mut times = Vec::new();
        let mut seq = first;
        'chunks: for c in r.chunks.iter().skip(at) {
            let mut k = (seq - c.first) as usize;
            while k < c.ends.len() && seq <= r.durable {
                let start = if k == 0 { 0 } else { c.ends[k - 1] };
                let end = c.ends[k];
                if !out.is_empty() && out.len() + (end - start) > max {
                    break 'chunks;
                }
                out.extend_from_slice(&c.data[start..end]);
                times.push(c.times[k]);
                seq += 1;
                k += 1;
            }
            if seq > r.durable {
                break;
            }
        }
        Next::Records(first, times, out)
    }

    /// Waits until a write after `cursor` is on disk, the feed starts over,
    /// or `timeout` passes.
    fn wait(&self, cursor: u64, epoch: u64, timeout: Duration) {
        let r = lock(&self.ring);
        if r.durable > cursor || r.epoch != epoch {
            return;
        }
        let _ = self
            .cv
            .wait_timeout(r, timeout)
            .unwrap_or_else(|e| e.into_inner());
    }
}

/// The sink a server with replicas writes through: its file, and the feed
/// they are sent from. A write enters the feed as it is appended and
/// becomes sendable once the file's fsync has covered it.
pub struct Tee {
    file: Box<dyn Sink>,
    feed: Arc<Feed>,
}

impl Sink for Tee {
    fn append(&mut self, bytes: &[u8]) -> fenec_core::error::Result<()> {
        self.file.append(bytes)
    }
    fn record(&mut self, seq: u64, bytes: &[u8]) -> fenec_core::error::Result<()> {
        self.file.record(seq, bytes)?;
        self.feed.push(seq, bytes);
        Ok(())
    }
    fn rewrite(&mut self, bytes: &[u8]) -> fenec_core::error::Result<()> {
        // The image holds every write, and is fsynced before it replaces
        // the file.
        self.file.rewrite(bytes)?;
        self.feed.mark_durable(self.feed.seq());
        Ok(())
    }
    fn sync(&mut self) -> fenec_core::error::Result<()> {
        let upto = self.feed.seq();
        self.file.sync()?;
        self.feed.mark_durable(upto);
        Ok(())
    }
    fn flush(&mut self) -> fenec_core::error::Result<Option<Durability>> {
        let upto = self.feed.seq();
        match self.file.flush()? {
            None => {
                self.feed.mark_durable(upto);
                Ok(None)
            }
            Some(durable) => {
                let feed = Arc::clone(&self.feed);
                Ok(Some(Box::new(move || {
                    durable()?;
                    feed.mark_durable(upto);
                    Ok(())
                })))
            }
        }
    }
}

/// Opens a file for a server that has replicas, or is one, with its writes
/// going through a [`Tee`] to a feed of `buffer` bytes.
///
/// The file is fsynced as it is: a process that died leaves bytes it wrote
/// but never synced, and those are the first a replica could be sent.
pub fn open(path: &str, buffer: usize) -> fenec_core::error::Result<(Database, Arc<Feed>)> {
    let (mut file, existing) = FileSink::open(path)?;
    file.sync_existing()?;
    let feed = Feed::new(buffer);
    let mut db = Database::with_sink(Box::new(Tee {
        file: Box::new(file),
        feed: Arc::clone(&feed),
    }));
    if existing.len() > MAGIC.len() {
        db.load(&existing)?;
    }
    feed.start(db.change_seq());
    Ok((db, feed))
}

// ------------------------------------------------------------- the primary

/// A server's part in replication: the token replicas and operators
/// present, the feed it serves replicas from, and the primary it follows.
pub struct Replication {
    token: String,
    feed: Option<Arc<Feed>>,
    follower: Option<Arc<Follower>>,
    streams: Mutex<Vec<Stream>>,
}

/// A replica being fed, as the primary's status lists it.
#[derive(Clone)]
struct Stream {
    key: u64,
    peer: String,
    sent: u64,
    image: bool,
}

impl Replication {
    pub fn new(
        token: String,
        feed: Option<Arc<Feed>>,
        follower: Option<Arc<Follower>>,
    ) -> Arc<Replication> {
        Arc::new(Replication {
            token,
            feed,
            follower,
            streams: Mutex::new(Vec::new()),
        })
    }

    fn authorized(&self, req: &Request) -> bool {
        let given = req
            .header("authorization")
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");
        constant_eq(given.as_bytes(), self.token.as_bytes())
    }
}

/// `/_replication/...`, answered here. `None` means the request was a
/// stream, which took the connection over and has ended.
pub fn handle(
    out: &mut TcpStream,
    db: &Arc<RwLock<Database>>,
    repl: &Replication,
    req: &Request,
) -> Option<Response> {
    use crate::http::Method;
    if !repl.authorized(req) {
        return Some(
            Response::error(401, "invalid or missing replication token")
                .header("WWW-Authenticate", "Bearer"),
        );
    }
    match (req.method, req.segments().as_slice()) {
        (Method::Get, ["_replication"]) => {
            stream(out, db, repl, req);
            None
        }
        (Method::Get, ["_replication", "status"]) => Some(status(db, repl)),
        (Method::Post, ["_replication", "promote"]) => Some(match &repl.follower {
            None => Response::error(409, "this server is not a replica"),
            Some(f) => match f.promote(fresh_id()) {
                Ok((seq, id)) => Response::json(
                    200,
                    format!("{{\"promoted\":true,\"seq\":{seq},\"history\":\"{id:016x}\"}}"),
                ),
                Err(e) => Response::error(500, &e.to_string()),
            },
        }),
        _ => Some(Response::error(404, &format!("path `{}`", req.path))),
    }
}

fn status(db: &Arc<RwLock<Database>>, repl: &Replication) -> Response {
    let (seq, history, following) = {
        let g = db.read().unwrap_or_else(|e| e.into_inner());
        (g.change_seq(), g.history().current(), g.history().following)
    };
    let mut out = format!(
        "{{\"role\":\"{}\",\"seq\":{seq},\"history\":\"{history:016x}\"",
        if following { "replica" } else { "primary" }
    );
    if let Some(feed) = &repl.feed {
        out.push_str(&format!(",\"durable\":{},\"replicas\":[", feed.durable()));
        for (i, s) in lock(&repl.streams).iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("{\"peer\":");
            fenec_core::json::escape_into(&mut out, &s.peer);
            out.push_str(&format!(",\"sent\":{},\"image\":{}}}", s.sent, s.image));
        }
        out.push(']');
    }
    if let Some(f) = &repl.follower {
        let st = lock(&f.state).clone();
        out.push_str(",\"primary\":");
        fenec_core::json::escape_into(&mut out, f.upstream.url());
        out.push_str(&format!(
            ",\"connected\":{},\"primary_durable\":{},\"behind\":{},\"last_contact_ms\":{},\"images\":{},\"reconnects\":{}",
            st.connected,
            st.primary_durable,
            st.primary_durable.saturating_sub(seq),
            st.contact.map_or(-1, |t| t.elapsed().as_millis() as i64),
            st.images,
            st.reconnects,
        ));
        if let Some(e) = &st.error {
            out.push_str(",\"error\":");
            fenec_core::json::escape_into(&mut out, e);
        }
    }
    out.push('}');
    Response::json(200, out)
}

fn message(out: &mut impl Write, tag: u8, parts: &[&[u8]]) -> io::Result<()> {
    let len: usize = parts.iter().map(|p| p.len()).sum();
    let mut head = [0u8; 9];
    head[0] = tag;
    head[1..].copy_from_slice(&(len as u64).to_le_bytes());
    out.write_all(&head)?;
    for p in parts {
        out.write_all(p)?;
    }
    Ok(())
}

fn u64s(values: &[u64]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// Feeds a replica: a hello, an image when it needs one, then the writes as
/// they reach the disk. It takes the connection over and returns when the
/// replica goes away or has to start again.
fn stream(out: &mut TcpStream, db: &Arc<RwLock<Database>>, repl: &Replication, req: &Request) {
    let Some(feed) = &repl.feed else {
        let _ = Response::error(409, "this server feeds no replicas").write(out, false, false);
        return;
    };
    let param = |k: &str| {
        req.query
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.as_str())
    };
    let since: u64 = param("since").and_then(|v| v.parse().ok()).unwrap_or(0);
    let history = param("history")
        .and_then(|v| u64::from_str_radix(v, 16).ok())
        .unwrap_or(0);
    let force = param("image") == Some("1");

    // Where the replica stands is judged against the database and the feed
    // at one moment, under the read lock: a write in between would leave
    // the image or the cursor one record off.
    let (seq, lineage, epoch, image) = {
        let g = db.read().unwrap_or_else(|e| e.into_inner());
        let now = g.change_seq();
        let epoch = feed.epoch();
        let go_on = !force && g.history().continues(history, since, now) && feed.serves(since);
        if go_on {
            (since, g.history().clone(), epoch, None)
        } else {
            (now, g.history().clone(), epoch, Some(g.snapshot()))
        }
    };
    // An image goes out once the primary's disk holds all of it.
    if image.is_some() {
        let deadline = Instant::now() + DURABLE_WAIT;
        while feed.durable() < seq {
            if Instant::now() > deadline {
                let _ = Response::error(503, "the primary's disk has not caught up")
                    .write(out, false, false);
                return;
            }
            feed.wait(seq - 1, epoch, Duration::from_millis(100));
        }
    }

    let _ = out.set_write_timeout(Some(SILENCE));
    let head = "HTTP/1.1 200 OK\r\nContent-Type: application/x-fenec-replication\r\n\
                Cache-Control: no-cache, no-transform\r\nConnection: close\r\n\
                X-Accel-Buffering: no\r\n\r\n";
    let key = fresh_id();
    lock(&repl.streams).push(Stream {
        key,
        peer: out.peer_addr().map(|a| a.to_string()).unwrap_or_default(),
        sent: seq,
        image: image.is_some(),
    });
    let _gone = Unlist(repl, key);

    let mut hello = vec![VERSION, image.is_some() as u8];
    hello.extend_from_slice(&u64s(&[seq, feed.durable(), now_ms()]));
    hello.extend_from_slice(&lineage.encode());
    let mut w = io::BufWriter::with_capacity(MESSAGE + 64, &*out);
    let opened = w.write_all(head.as_bytes()).and_then(|_| {
        message(&mut w, b'H', &[&hello])?;
        if let Some(image) = &image {
            message(&mut w, b'I', &[image])?;
        }
        w.flush()
    });
    drop(image);
    if opened.is_err() {
        return;
    }

    let mut cursor = seq;
    loop {
        match feed.next(cursor, epoch, MESSAGE) {
            Next::Records(first, times, records) => {
                let n = times.len() as u64;
                let sent = message(&mut w, b'W', &[&u64s(&[first, n]), &u64s(&times), &records])
                    .and_then(|_| w.flush());
                if sent.is_err() {
                    return;
                }
                cursor = first + n - 1;
                if let Some(s) = lock(&repl.streams).iter_mut().find(|s| s.key == key) {
                    s.sent = cursor;
                }
            }
            Next::Nothing => {
                feed.wait(cursor, epoch, ALIVE);
                if feed.durable() <= cursor {
                    let alive = u64s(&[feed.seq(), feed.durable(), now_ms()]);
                    if message(&mut w, b'K', &[&alive])
                        .and_then(|_| w.flush())
                        .is_err()
                    {
                        return;
                    }
                }
            }
            Next::Behind => {
                let _ = message(
                    &mut w,
                    b'E',
                    &[b"fell behind the primary's buffer: connect again for an image"],
                )
                .and_then(|_| w.flush());
                return;
            }
            Next::Moved => {
                let _ = message(&mut w, b'E', &[b"the primary started over: connect again"])
                    .and_then(|_| w.flush());
                return;
            }
        }
    }
}

/// Takes a finished stream off the status list.
struct Unlist<'a>(&'a Replication, u64);
impl Drop for Unlist<'_> {
    fn drop(&mut self) {
        lock(&self.0.streams).retain(|s| s.key != self.1);
    }
}

// ------------------------------------------------------ the receiving side

/// A primary to be fed from: its address, and the token it asks for.
#[derive(Clone)]
pub struct Upstream {
    url: String,
    addr: String,
    token: String,
}

/// A message of the stream, decoded.
pub enum Message {
    /// Where the stream starts: `seq`, and the primary's disk at `durable`.
    /// An [`Message::Image`] follows when `image` is set.
    Hello {
        image: bool,
        seq: u64,
        durable: u64,
        lineage: Vec<(u64, u64)>,
    },
    Image(Vec<u8>),
    /// Writes numbered on from `first`, each with the time it was appended.
    Writes {
        first: u64,
        times: Vec<u64>,
        records: Vec<u8>,
    },
    Alive {
        durable: u64,
    },
    End(String),
}

impl Upstream {
    /// `url` is the primary's HTTP address, `http://host:port`. There is no
    /// TLS here either: across an open network, a tunnel carries it.
    pub fn new(url: &str, token: String) -> std::result::Result<Upstream, String> {
        let rest = url.strip_prefix("http://").ok_or_else(|| {
            format!("a primary is http://host:port, not `{url}` (there is no TLS)")
        })?;
        let addr = rest.trim_end_matches('/').to_string();
        if addr.is_empty() || addr.contains('/') {
            return Err(format!("a primary is http://host:port, not `{url}`"));
        }
        Ok(Upstream {
            url: url.trim_end_matches('/').to_string(),
            addr,
            token,
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// Opens the stream for a database `since` changes in on `history`;
    /// `image` asks for one whatever the positions say.
    pub fn open(&self, since: u64, history: u64, image: bool) -> io::Result<Receiver> {
        let s = connect(&self.addr)?;
        s.set_read_timeout(Some(SILENCE))?;
        s.set_write_timeout(Some(SILENCE))?;
        let request = format!(
            "GET /_replication?since={since}&history={history:016x}{} HTTP/1.1\r\n\
             Host: {}\r\nAuthorization: Bearer {}\r\n\r\n",
            if image { "&image=1" } else { "" },
            self.addr,
            self.token
        );
        (&s).write_all(request.as_bytes())?;
        let conn = s.try_clone()?;
        let mut r = BufReader::with_capacity(MESSAGE + 64, s);
        let (status, length) = read_head(&mut r)?;
        if status != 200 {
            let mut body = vec![0u8; length.unwrap_or(0).min(64 << 10)];
            r.read_exact(&mut body)?;
            return Err(io::Error::other(format!(
                "the primary answered {status}: {}",
                String::from_utf8_lossy(&body)
            )));
        }
        Ok(Receiver { r, conn })
    }
}

/// An open stream.
pub struct Receiver {
    r: BufReader<TcpStream>,
    conn: TcpStream,
}

impl Receiver {
    /// A handle to cut the stream from another thread: a read waiting on
    /// it returns.
    pub fn handle(&self) -> io::Result<TcpStream> {
        self.conn.try_clone()
    }

    pub fn receive(&mut self) -> io::Result<Message> {
        let bad = || io::Error::other("the primary sent a message it should not");
        let (tag, body) = read_message(&mut self.r)?;
        Ok(match tag {
            b'H' => {
                if body.len() < 26 || body[0] != VERSION {
                    return Err(io::Error::other("the primary speaks another protocol"));
                }
                Message::Hello {
                    image: body[1] == 1,
                    seq: u64_at(&body, 2),
                    durable: u64_at(&body, 10),
                    lineage: History::decode(&body[26..])
                        .map_err(|e| io::Error::other(e.to_string()))?
                        .lineage,
                }
            }
            b'I' => Message::Image(body),
            b'W' => {
                let n = body.get(8..16).ok_or_else(bad)?;
                let n = u64::from_le_bytes(n.try_into().unwrap()) as usize;
                let at = 16 + 8 * n;
                if body.len() < at {
                    return Err(bad());
                }
                Message::Writes {
                    first: u64_at(&body, 0),
                    times: (0..n).map(|i| u64_at(&body, 16 + 8 * i)).collect(),
                    records: body[at..].to_vec(),
                }
            }
            b'K' if body.len() >= 24 => Message::Alive {
                durable: u64_at(&body, 8),
            },
            b'E' => Message::End(String::from_utf8_lossy(&body).into_owned()),
            _ => return Err(bad()),
        })
    }
}

/// A replica's side: connects to the primary, applies what it is sent, and
/// connects again when the stream ends -- until it is promoted.
pub struct Follower {
    upstream: Upstream,
    db: Arc<RwLock<Database>>,
    /// This server's own feed, when it has replicas of its own.
    feed: Option<Arc<Feed>>,
    /// Push each write applied to disk before the next: `--sync always`,
    /// whose fsyncs happen on the write path the replica's writes bypass.
    sync_each: bool,
    state: Mutex<State>,
    stop: AtomicBool,
    conn: Mutex<Option<TcpStream>>,
    ended: (Mutex<bool>, Condvar),
}

#[derive(Clone, Default)]
struct State {
    connected: bool,
    /// The last write on the primary's disk, as it last said.
    primary_durable: u64,
    contact: Option<Instant>,
    images: u64,
    reconnects: u64,
    error: Option<String>,
}

impl Follower {
    pub fn new(
        url: &str,
        token: String,
        db: Arc<RwLock<Database>>,
        feed: Option<Arc<Feed>>,
        sync_each: bool,
    ) -> std::result::Result<Arc<Follower>, String> {
        Ok(Arc::new(Follower {
            upstream: Upstream::new(url, token)?,
            db,
            feed,
            sync_each,
            state: Mutex::new(State::default()),
            stop: AtomicBool::new(false),
            conn: Mutex::new(None),
            // Nothing to wait for until `run` starts.
            ended: (Mutex::new(true), Condvar::new()),
        }))
    }

    /// Follows until promoted. Every stream that ends is connected again,
    /// sooner after a clean end than after a failure.
    pub fn run(&self) {
        *lock(&self.ended.0) = false;
        let mut force_image = false;
        let mut pause = Duration::from_millis(100);
        while !self.stop.load(Ordering::SeqCst) {
            let r = self.session(&mut force_image);
            {
                let mut st = lock(&self.state);
                st.connected = false;
                if let Err(e) = &r {
                    if !self.stop.load(Ordering::SeqCst) {
                        st.error = Some(e.to_string());
                    }
                }
            }
            // A replica whose own disk failed takes nothing more; it answers
            // reads from what it holds, as a primary does after one.
            let failed = self
                .db
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .failure()
                .map(String::from);
            if let Some(f) = failed {
                eprintln!("replica stopped: {f}");
                break;
            }
            if self.stop.load(Ordering::SeqCst) {
                break;
            }
            if let Err(e) = &r {
                eprintln!("replication: {e}; connecting again");
            }
            lock(&self.state).reconnects += 1;
            let until = Instant::now() + pause;
            while Instant::now() < until && !self.stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(20));
            }
            pause = if r.is_ok() {
                Duration::from_millis(100)
            } else {
                (pause * 2).min(Duration::from_secs(5))
            };
        }
        *lock(&self.ended.0) = true;
        self.ended.1.notify_all();
    }

    fn session(&self, force_image: &mut bool) -> io::Result<()> {
        let (since, history) = {
            let g = self.db.read().unwrap_or_else(|e| e.into_inner());
            (g.change_seq(), g.history().current())
        };
        let mut stream = self.upstream.open(since, history, *force_image)?;
        *lock(&self.conn) = Some(stream.handle()?);
        if self.stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        let Message::Hello {
            image,
            seq,
            durable,
            lineage,
        } = stream.receive()?
        else {
            return Err(io::Error::other("the primary did not say hello"));
        };
        if image {
            let Message::Image(bytes) = stream.receive()? else {
                return Err(io::Error::other("the primary promised an image"));
            };
            // Loaded before the lock is taken: reads go on meanwhile.
            let mut fresh = Database::new();
            fresh.load(&bytes).map_err(io::Error::other)?;
            let mut g = self.db.write().unwrap_or_else(|e| e.into_inner());
            g.adopt(fresh, &bytes).map_err(io::Error::other)?;
            g.follow(lineage).map_err(io::Error::other)?;
            if let Some(feed) = &self.feed {
                feed.start(g.change_seq());
            }
            debug_assert_eq!(g.change_seq(), seq);
            lock(&self.state).images += 1;
            *force_image = false;
        } else {
            let mut g = self.db.write().unwrap_or_else(|e| e.into_inner());
            if g.change_seq() != since {
                return Err(io::Error::other("the replica moved while it connected"));
            }
            g.follow(lineage).map_err(io::Error::other)?;
        }
        {
            let mut st = lock(&self.state);
            st.connected = true;
            st.primary_durable = durable;
            st.contact = Some(Instant::now());
            st.error = None;
        }

        loop {
            match stream.receive()? {
                Message::Writes { first, records, .. } => {
                    let mut g = self.db.write().unwrap_or_else(|e| e.into_inner());
                    if first != g.change_seq() + 1 {
                        *force_image = true;
                        return Err(io::Error::other(format!(
                            "the primary sent write {first}, the replica is at {}",
                            g.change_seq()
                        )));
                    }
                    if let Err(e) = g.apply(&records) {
                        *force_image = true;
                        return Err(io::Error::other(format!("could not apply a write: {e}")));
                    }
                    let at = g.change_seq();
                    let durable = if self.sync_each {
                        g.flush().map_err(io::Error::other)?
                    } else {
                        None
                    };
                    drop(g);
                    // The fsync runs without the lock, as a primary's does.
                    if let Some(durable) = durable {
                        if let Err(e) = durable() {
                            self.db.write().unwrap_or_else(|p| p.into_inner()).fail(&e);
                            return Err(io::Error::other(e.to_string()));
                        }
                    }
                    let mut st = lock(&self.state);
                    st.primary_durable = st.primary_durable.max(at);
                    st.contact = Some(Instant::now());
                }
                Message::Alive { durable } => {
                    let mut st = lock(&self.state);
                    st.primary_durable = durable;
                    st.contact = Some(Instant::now());
                }
                Message::End(why) => return Err(io::Error::other(why)),
                _ => return Err(io::Error::other("the primary sent a message it should not")),
            }
        }
    }

    /// Stops following and starts taking writes, on a history of its own:
    /// `id`, forked where the replica stands. Returns that change and `id`.
    pub fn promote(&self, id: u64) -> fenec_core::error::Result<(u64, u64)> {
        self.halt();
        let mut g = self.db.write().unwrap_or_else(|e| e.into_inner());
        g.fork(id)?;
        g.sync()?;
        Ok((g.change_seq(), id))
    }

    /// Stops following and stays a replica: started again, it follows on
    /// from where it stopped.
    pub fn halt(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(s) = lock(&self.conn).take() {
            let _ = s.shutdown(Shutdown::Both);
        }
        let mut ended = lock(&self.ended.0);
        while !*ended {
            ended = self
                .ended
                .1
                .wait_timeout(ended, Duration::from_millis(50))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Whether the replica is connected, and how far behind the primary's
    /// disk it is, for tests and measurement.
    pub fn progress(&self) -> (bool, u64) {
        let st = lock(&self.state);
        (st.connected, st.primary_durable)
    }

    /// Images taken so far.
    pub fn images(&self) -> u64 {
        lock(&self.state).images
    }
}

fn connect(addr: &str) -> io::Result<TcpStream> {
    let mut last = io::Error::new(io::ErrorKind::NotFound, format!("{addr} does not resolve"));
    for a in addr.to_socket_addrs()? {
        match TcpStream::connect_timeout(&a, Duration::from_secs(5)) {
            Ok(s) => {
                let _ = s.set_nodelay(true);
                return Ok(s);
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// The status line and the length of the body, when it has one.
fn read_head(r: &mut impl BufRead) -> io::Result<(u16, Option<usize>)> {
    let mut line = String::new();
    r.read_line(&mut line)?;
    let status = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| io::Error::other(format!("not an HTTP answer: {line:?}")))?;
    let mut length = None;
    let mut total = 0;
    loop {
        line.clear();
        let n = r.read_line(&mut line)?;
        total += n;
        if n == 0 || total > crate::http::MAX_HEADER_BYTES {
            return Err(io::Error::other("the answer's head did not end"));
        }
        let l = line.trim_end();
        if l.is_empty() {
            return Ok((status, length));
        }
        if let Some((k, v)) = l.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                length = v.trim().parse().ok();
            }
        }
    }
}

fn read_message(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 9];
    r.read_exact(&mut head)?;
    let len = u64::from_le_bytes(head[1..].try_into().unwrap());
    // The primary is trusted -- the replica presented it the token -- but
    // a stream that lost its place reads a length out of the middle of an
    // image: that one is not allocated.
    if len > 1 << 40 {
        return Err(io::Error::other("a message longer than any database"));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    Ok((head[0], body))
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(n: usize) -> Vec<u8> {
        // [kind][cid][length][body]
        let mut r = vec![3, 1];
        fenec_core::codec::put_uvarint(&mut r, n as u64);
        r.extend(std::iter::repeat_n(0xab, n));
        r
    }

    #[test]
    fn only_what_is_on_disk_is_sent() {
        let feed = Feed::new(1 << 20);
        feed.start(10);
        let epoch = feed.epoch();
        for seq in 11..=15 {
            feed.push(seq, &record(4));
        }
        // Appended, not yet durable: nothing to send.
        assert!(matches!(feed.next(10, epoch, MESSAGE), Next::Nothing));
        feed.mark_durable(13);
        match feed.next(10, epoch, MESSAGE) {
            Next::Records(first, times, bytes) => {
                assert_eq!(first, 11);
                assert_eq!(times.len(), 3);
                assert_eq!(bytes, record(4).repeat(3));
            }
            _ => panic!("records expected"),
        }
        assert!(matches!(feed.next(13, epoch, MESSAGE), Next::Nothing));
        // A mark past what was appended counts only what was.
        feed.mark_durable(99);
        assert_eq!(feed.durable(), 15);
    }

    #[test]
    fn a_small_buffer_drops_the_oldest_and_sends_behind() {
        // 8 000 bytes, kept in chunks of 1 000: three records each.
        let feed = Feed::new(8_000);
        feed.start(0);
        let epoch = feed.epoch();
        let big = record(250);
        let n = 200;
        for seq in 1..=n {
            feed.push(seq, &big);
        }
        feed.mark_durable(n);
        assert!(!feed.serves(0));
        assert!(matches!(feed.next(0, epoch, MESSAGE), Next::Behind));
        // The newest chunk holds the last two writes (200 = 66 x 3 + 2), and
        // nine whole ones fit beside it: 9 x 759 + 506 = 7 337 bytes, where
        // a tenth would make 8 096.
        let oldest = (0..n).find(|&s| feed.serves(s)).unwrap();
        assert_eq!(oldest, n - 29);
        assert!(matches!(
            feed.next(oldest - 1, epoch, MESSAGE),
            Next::Behind
        ));
        let Next::Records(first, times, _) = feed.next(oldest, epoch, MESSAGE) else {
            panic!("records expected");
        };
        assert_eq!((first, times.len()), (oldest + 1, 29));
        // A message stops at its size, across the line between two chunks
        // as anywhere.
        let across = oldest + 2;
        let Next::Records(first, times, bytes) = feed.next(across, epoch, 4 * big.len()) else {
            panic!("records expected");
        };
        assert_eq!((first, times.len()), (across + 1, 4));
        assert_eq!(bytes, big.repeat(4));
        // Starting over ends every stream begun before.
        feed.start(n);
        assert!(matches!(feed.next(n - 10, epoch, MESSAGE), Next::Moved));
    }
}
