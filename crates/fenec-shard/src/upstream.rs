//! The router's side of the conversation with a node: just enough HTTP/1.1
//! client to forward a request and read the answer.
//!
//! The same scope as the server-side parser: a request with
//! `Content-Length`, a response that is either `Content-Length` delimited
//! or read to the close (an SSE stream). Nodes never send a chunked body,
//! so there is none to decode.
//!
//! Connections are kept per node address and reused, and a node closes one
//! silent past its `--idle-timeout`. So a pooled connection is looked at
//! before it is used (`open`: a read that does not wait) and let go of
//! when the node has closed or reset it. Before the look, macOS answered a
//! request after a minute of quiet 502: some 60 s after the node closed the
//! connection (`net.inet.tcp.fin_timeout`) it is reset, and `setsockopt`
//! there refuses a socket that can neither send nor receive with EINVAL,
//! which `set_read_timeout` returned before a byte was written -- an error
//! the retry, which knew EOF, a broken pipe and a reset, did not know.
//!
//! The look cannot see a close on its way: the node's idle timeout firing
//! as the request goes out. What may be sent again on a fresh connection
//! then turns on how far the exchange got (`Stage`), not on the error,
//! which each system names its own way (EINVAL, EPIPE and ECONNRESET for
//! one reset connection). Failing before the request's last byte
//! was handed to the system, the node never read a whole request and ran
//! nothing: it is sent again, whatever it is. Failing after it, the
//! connection ending before the first byte of an answer, the node may have
//! closed it as the request came in or run the request and gone down: it is
//! sent again only where running it twice is harmless -- a `GET`, `HEAD` or
//! `OPTIONS`, which a node never writes for, or a write under an
//! `Idempotency-Key`, which it makes once. Anything else is the client's
//! 502: retried here, a `put` that had landed was made twice.
//!
//! A connection's name is looked up within the time its connect may take
//! ([`fenec_http::dial::resolve`]), on a thread of its own that the dial
//! walks away from past it: `getaddrinfo` takes no timeout, and a node
//! recorded by a name its resolver was slow to refuse -- a container gone
//! -- held every dial to it for the resolver's 5 s, a lease grant's among
//! them, whatever the pool's timeout said. An address written out is read
//! where it stands, with no thread.
//!
//! A request the node may hold (`Fenec-Wait`) watches its client while it
//! waits for the head ([`answered_or_gone`]), and passes on to the node
//! what the client did, as the node would read it from a client of its
//! own: the client's end -- closed, or half-closed and still reading -- is
//! the node's connection shut for writing, whose end the node's held claim
//! sees before its next run and hands its row to the next claim, and the
//! node's answer goes on to the client as any; a reset, which can read
//! nothing more, lets go of the node's connection. Blocked in the head's
//! read, the router never looked at its client, and a claim whose client
//! had gone took the next job, leased to nobody until its lease ran out,
//! where asked of the node itself it passed it on. Each hop does the same,
//! so a region's router relaying such a request passes the end on to the
//! home router, and the home router to the node.
//!
//! A node that dies with no reset -- a machine gone, a network cut --
//! answers nothing and closes nothing, and a request out to it waited for
//! the pool's timeout (`--upstream-timeout`, 60 s, the deploy kit's 10)
//! even once its tenant was failed over and its new primary answered: in
//! V5's drill the writers sat idle for seconds past the failover. So the
//! pool knows each connection it made and what is out on it ([`Flight`]):
//! a failover ends what is out to the node for each tenant it moved
//! ([`Pool::cut`]) by shutting the connection down, which wakes the thread
//! waiting on it, and the router answers that request by the rules above,
//! sooner -- sent again to the tenant's new primary where the node cannot
//! have run it or running it twice is harmless, a 502 saying it may have
//! run otherwise (`Router::after_cut`). Only a request whose answer has not
//! begun is cut: once its head is in, its answer is the node's, and a
//! stream's is cut, so its client opens it again where its tenant is. A
//! forward still dialing the node -- a machine gone answers no connect --
//! gives up within a try of a second once a failover has moved a tenant
//! ([`Pool::connect_watching`]).

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Response header ceiling, same as the server side's.
const MAX_HEAD: usize = fenec_http::http::MAX_HEADER_BYTES;

/// Idle connections kept per node. Beyond it a returned connection is
/// closed: the pool bounds descriptors, the router's thread count bounds
/// concurrency.
const MAX_IDLE: usize = 32;

pub struct Pool {
    /// Each node's connections by its address: those idle, and every one
    /// made and not yet closed, which a failover's cut looks among
    /// ([`Pool::cut`]). Under the lock a request takes anyway to take a
    /// pooled connection and to put it back: a connection is put among
    /// them as it is made and its flight marked closed as it goes, and a
    /// request marks what it does on its connection's flight alone, so no
    /// lock is taken a request more.
    idle: Mutex<HashMap<String, Kept>>,
    timeout: Duration,
    /// Moved by every failover's cut ([`Pool::cut`]), the bucket of the
    /// tenant's mark, once the directory places the tenant elsewhere and
    /// before the cut looks at a connection. A forward reads its own before
    /// it reads the directory and again as its request goes out
    /// ([`Flying`]): moved between the two, the tenant may have been failed
    /// over after the directory was read and before the request was out
    /// where a cut could see it -- placed on the node a moment before,
    /// written to it a moment after, and left to wait for the timeout -- so
    /// the directory is read again rather than the request sent. A
    /// failover's alone: a forward reads the same number twice, a load of
    /// a line nothing else writes. One count for every tenant, any failover
    /// sent the forwards straddling it to the directory again, a healthy
    /// connection let go of each.
    moved: [AtomicU64; MOVED_BUCKETS],
}

#[derive(Default)]
struct Kept {
    idle: Vec<Idle>,
    /// Every connection made to the node and not yet found closed:
    /// those closed are let go of as the next is made, and as a cut looks.
    flights: Vec<Arc<Flight>>,
}

/// A pooled connection and the read timeout it was left with, so that a
/// request waiting as long sets none: the two `setsockopt`s every exchange
/// made took 0.67 us, more than the look (`open`) made in their place.
struct Idle {
    conn: Conn,
    read_timeout: Duration,
    /// Set at the connect, and only by a read replica's read after
    /// ([`Pool::send_within`]): the pool's timeout otherwise, so a request
    /// sets it on no connection a replica's read has not used.
    write_timeout: Duration,
}

pub struct Answer {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// The connection the rest comes on, `None` once
    /// [`Answer::read_body_kept`] has read it and handed it back.
    reader: Option<BufReader<Conn>>,
    addr: String,
    reusable: bool,
    read_timeout: Duration,
    write_timeout: Duration,
    /// What the forward it answers was known by, which a stream is asked
    /// again by ([`Answer::into_stream`]).
    flying: Flying,
}

/// What a failover knows a forward by: its tenant, as a mark, and the
/// cuts of its mark's bucket ([`Pool::moved`]) there had been as the
/// directory placed it.
#[derive(Clone, Copy)]
pub struct Flying {
    mark: u64,
    seen: u64,
}

impl Flying {
    /// A request no failover ends: the router's own calls to a node, a
    /// relay to a region's routers, a read replica's read.
    pub const NONE: Flying = Flying { mark: 0, seen: 0 };

    /// A forward of `tenant`'s request through `pool`, made before the
    /// directory is read: the cuts its bucket has had so far. Acquired, so
    /// a count a failover's cut moved is read with the directory as that
    /// failover left it: the failover places its tenants first and cuts
    /// after ([`Pool::cut`]).
    pub fn of(tenant: &str, pool: &Pool) -> Flying {
        let mark = mark_of(tenant);
        Flying {
            mark,
            seen: pool.moved[bucket(mark)].load(Ordering::Acquire),
        }
    }
}

/// `tenant` as a flight carries it: a hash of its name, keyed by a number
/// this process drew ([`seed`]), its three low bits left for the state and
/// its fourth set, so that no tenant's is zero, which marks no forward. Two
/// tenants on one node sharing a mark is a request of one cut by the
/// other's failover: sent again where the directory places it, which is
/// its own node, where it may go again; answered 502, where its write may
/// have run -- an answer the node had on its way. So the multiply chain
/// ends in MurmurHash3's finisher and the mark keeps its high bits: kept
/// low, `(h | 1) << 3` dropped the top three, the low 61 bits of a product
/// depend on the low 61 of what is multiplied alone, and two names of a
/// multiple of 8 bytes differing only in their last byte's top three bits
/// -- `tenant-1` and `tenant-q`, `shop-002` and `shop-00r` -- always
/// shared one: 603 592 groups of them among the 2.1 million names `shop`
/// and four of `[a-z0-9_-]`, none this way. Keyed, they cannot be made to
/// share one on purpose by someone naming tenants.
fn mark_of(tenant: &str) -> u64 {
    const K: u64 = 0x517c_c1b7_2722_0a95;
    let mut h = seed() ^ tenant.len() as u64;
    for chunk in tenant.as_bytes().chunks(8) {
        let mut word = [0u8; 8];
        word[..chunk.len()].copy_from_slice(chunk);
        h = (h.rotate_left(5) ^ u64::from_le_bytes(word)).wrapping_mul(K);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^= h >> 33;
    (h & !STATE) | (STATE + 1)
}

/// The key every mark of this process is hashed under: drawn once, from
/// the standard library's own random keys (`RandomState`).
fn seed() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    static SEED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *SEED.get_or_init(|| {
        std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish()
    })
}

/// Buckets of the cut counts a forward reads ([`Pool::moved`]), by a mark's
/// top bits: a failover moves its own tenants' bucket alone, so a forward
/// straddling an unrelated one goes out as it would have.
const MOVED_BUCKETS: usize = 64;

fn bucket(mark: u64) -> usize {
    (mark >> (64 - MOVED_BUCKETS.trailing_zeros())) as usize
}

/// Whether `a`'s and `b`'s requests count in one bucket of
/// [`Pool::moved`], this process's marks being keyed: how a test finds a
/// tenant whose failover moves another's count.
#[doc(hidden)]
pub fn same_bucket(a: &str, b: &str) -> bool {
    bucket(mark_of(a)) == bucket(mark_of(b))
}

/// What a connection is doing, in its flight's low bits beside the mark of
/// the tenant whose request is out on it.
const STATE: u64 = 7;
/// Made, or back in the pool: nothing is out.
const IDLE: u64 = 0;
/// A request going out, or its answer's head waited for: what a cut ends.
const WAITING: u64 = 1;
/// Its answer's head is in: the node's, never cut.
const ANSWERING: u64 = 2;
/// A stream copied to its client: cut, its client opens it again.
const STREAMING: u64 = 3;
/// A cut shutting the connection down: its owner waits before it closes,
/// so the descriptor the cut shuts is still the connection's.
const CUTTING: u64 = 4;
/// Shut down by a cut.
const CUT: u64 = 5;
/// Its descriptor closed, or about to be: no cut touches it.
const CLOSED: u64 = 6;

/// What a pooled connection is doing, shared with the pool's list of the
/// node's connections ([`Pool::cut`]): one word, the state in its low bits
/// beside the mark of the tenant whose request is out ([`mark_of`]), so a
/// cut compares and changes both at once -- read apart, a request of
/// another tenant out on the connection a moment later could be the one
/// cut. One allocation a connection, not a request; a request writes it
/// once as the pool hands the connection over ([`Pool::take`],
/// [`Pool::register`]) and compares it once as its answer begins.
struct Flight {
    word: AtomicU64,
    #[cfg(unix)]
    fd: std::os::fd::RawFd,
}

impl Flight {
    /// `s`'s flight, its word `word`.
    fn of(s: &TcpStream, word: u64) -> Flight {
        #[cfg(unix)]
        let fd = std::os::fd::AsRawFd::as_raw_fd(s);
        #[cfg(not(unix))]
        let _ = s;
        Flight {
            word: AtomicU64::new(word),
            #[cfg(unix)]
            fd,
        }
    }

    /// Its answer's head is in: whether it was before a cut, which then
    /// leaves it be. False, the cut came first and has shut it down.
    fn answering(&self, mark: u64) -> bool {
        self.word
            .compare_exchange(
                mark | WAITING,
                mark | ANSWERING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// The answer is a stream: a cut ends it from here. No cut writes the
    /// word while its head is in, so this is the owner's alone. Sequentially
    /// consistent with the read of [`Pool::moved`] after it, as the cut's
    /// count and its look at the word are: a cut that came while the head
    /// was read finds it here or is found by that read
    /// ([`Answer::into_stream`]). No lock of the pool's is taken here to
    /// order them by, as [`Pool::take`] orders a request going out: this
    /// is once a stream, not every request.
    fn streaming(&self) {
        let w = self.word.load(Ordering::Acquire);
        self.word.store((w & !STATE) | STREAMING, Ordering::SeqCst);
    }

    /// `mark`'s request, not a byte of it written, will not go out after
    /// all: whether the connection is the pool's again -- false, a cut has
    /// it already.
    fn rest(&self, mark: u64) -> bool {
        self.word
            .compare_exchange(mark | WAITING, IDLE, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// Whether a failed exchange was a cut's doing, once the cut is done.
    fn was_cut(&self) -> bool {
        loop {
            match self.word.load(Ordering::SeqCst) & STATE {
                CUTTING => std::thread::yield_now(),
                s => return s == CUT,
            }
        }
    }

    fn closed(&self) -> bool {
        self.word.load(Ordering::Relaxed) & STATE == CLOSED
    }

    /// Marked closed before the descriptor is, waiting out a cut shutting
    /// it down: closed under a cut, the descriptor's number could be a new
    /// connection's by the time the cut shut it.
    fn close(&self) {
        let mut w = self.word.load(Ordering::SeqCst);
        loop {
            if w & STATE == CUTTING {
                std::thread::yield_now();
                w = self.word.load(Ordering::SeqCst);
                continue;
            }
            match self
                .word
                .compare_exchange_weak(w, CLOSED, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return,
                Err(now) => w = now,
            }
        }
    }

    /// Ends `mark`'s request out on this connection, where its answer has
    /// not begun or is a stream: the connection shut down both ways, which
    /// wakes the thread reading or writing it -- a read meets the end, a
    /// write its refusal -- and tells a node still there, as a client gone
    /// would. What it ended: `WAITING`, `STREAMING`, or nothing.
    fn cut(&self, mark: u64) -> Option<u64> {
        for from in [WAITING, STREAMING] {
            let cutting = self.word.compare_exchange(
                mark | from,
                mark | CUTTING,
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
            if cutting.is_ok() {
                self.shut();
                self.word.store(mark | CUT, Ordering::SeqCst);
                return Some(from);
            }
        }
        None
    }

    #[cfg(unix)]
    fn shut(&self) {
        use std::os::fd::FromRawFd;
        // SAFETY: the descriptor is the connection's: its owner closes it
        // only once the word has left `CUTTING` (`close`), and it is never
        // closed here, the stream made over it not dropped.
        let s = std::mem::ManuallyDrop::new(unsafe { TcpStream::from_raw_fd(self.fd) });
        let _ = s.shutdown(std::net::Shutdown::Both);
    }

    /// No descriptor to shut where there is no `RawFd`: the request waits
    /// for the timeout, as before.
    #[cfg(not(unix))]
    fn shut(&self) {}
}

/// A connection to a node and its flight, marked closed as it is dropped,
/// before its descriptor is.
pub struct Conn {
    stream: TcpStream,
    flight: Arc<Flight>,
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&self.stream).read(buf)
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.flight.close();
    }
}

/// What a cut ended ([`Pool::cut`]): requests whose answer had not begun,
/// and streams.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cuts {
    pub waiting: usize,
    pub streams: usize,
}

/// What a failover did to a forward that ended short of its answer
/// ([`ended_by`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// Its cut shut the forward's connection down ([`Pool::cut`]):
    /// `whole`, the request had gone out whole, so the node may have run
    /// it.
    Cut { whole: bool },
    /// A failover moved a tenant of its mark's bucket after the directory
    /// placed it and before it went out where a cut could see it -- or
    /// while it connected: nothing of it was written, and the directory is
    /// read again, which may place it where it was.
    Moved,
}

impl std::fmt::Display for Ended {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Ended::Cut { .. } => "its node was failed over before it answered",
            Ended::Moved => "a failover moved its tenant's bucket before it went out",
        })
    }
}

impl std::error::Error for Ended {}

/// What a failover did to the forward whose failure `e` is, where one did.
pub fn ended_by(e: &io::Error) -> Option<Ended> {
    e.get_ref()?.downcast_ref::<Ended>().copied()
}

/// The failure of an exchange that ended at `stage` with `e`, or the cut's
/// where a failover's cut ended it.
#[cold]
#[inline(never)]
fn failed(flight: &Flight, stage: Stage, e: io::Error) -> Failure {
    if !flight.was_cut() {
        return Failure(stage, e);
    }
    let whole = !matches!(stage, Stage::Unsent | Stage::Unread);
    Failure(Stage::Cut, io::Error::other(Ended::Cut { whole }))
}

/// A cut's error `c`, of a request whose last byte never went out: a cut
/// of one sent whole ([`failed`]'s, the head waited for) is one unsent, its
/// request run nowhere; a move stays a move.
#[cold]
#[inline(never)]
fn unsent_cut(c: io::Error) -> io::Error {
    match ended_by(&c) {
        Some(Ended::Cut { whole: true }) => io::Error::other(Ended::Cut { whole: false }),
        _ => c,
    }
}

/// An answer whose body [`Answer::read_body_kept`] has read already.
#[cold]
#[inline(never)]
fn read_already() -> io::Error {
    bad("the answer's body was read already")
}

/// A forward a failover moved before it went out ([`Ended::Moved`]).
#[cold]
#[inline(never)]
fn moved_meanwhile() -> Failure {
    Failure(Stage::Cut, io::Error::other(Ended::Moved))
}

/// How far an exchange got before it failed, which is what decides whether
/// the request may be sent again (the module's comment).
enum Stage {
    /// Before the request's last byte was handed to the system: the node
    /// cannot have read a whole request, so it ran nothing.
    Unsent,
    /// The request went out whole and the connection ended before the
    /// first byte of an answer: the node may have run it.
    Unanswered,
    /// A write of the request timed out: the node took part of it and
    /// stopped reading. Sent again, it would wait as long.
    Unread,
    /// A timeout, or an answer cut short or malformed.
    Other,
    /// The client of a held request reset its connection before the
    /// answer began: the node's connection is let go of, and nothing is
    /// sent again or written.
    Abandoned,
    /// A failover's cut ended it, or moved its tenant before it went out
    /// ([`ended_by`]): never sent again on this node, whose tenant is placed
    /// elsewhere now.
    Cut,
}

struct Failure(Stage, io::Error);

/// What a held request's failure is once its client has reset its
/// connection ([`client_gone`]): nobody is left to answer, so the router
/// writes nothing and counts no node as out of reach.
#[derive(Debug)]
struct ClientGone;

impl std::fmt::Display for ClientGone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the client reset its connection while its request was held")
    }
}

impl std::error::Error for ClientGone {}

/// Whether `e` is a held request's client gone ([`answered_or_gone`]).
pub fn client_gone(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|e| e.is::<ClientGone>())
}

/// How a request to one of several routers failed (`Pool::send_tried`):
/// whether the one asked cannot have run it, so that the next may be
/// asked, or may have, so that it is not -- run twice, a `put` made a row
/// twice.
pub enum Tried {
    Unrun(io::Error),
    MayHaveRun(io::Error),
}

/// How a read sent to a read replica failed ([`Pool::send_within`]):
/// before the replica took the whole request -- not reached, or not
/// reading -- which is the node's, or after, its answer not come in time
/// or broken off, which may be that one read's own: its head comes once
/// its query has run, and a read slower than the router's
/// `--replica-timeout` looks just like a node that hangs.
pub enum Near {
    Unreached(io::Error),
    Unanswered(io::Error),
}

impl Tried {
    pub fn into_error(self) -> io::Error {
        match self {
            Tried::Unrun(e) | Tried::MayHaveRun(e) => e,
        }
    }
}

impl Pool {
    /// `timeout` bounds connecting and every read of an ordinary answer.
    pub fn new(timeout: Duration) -> Pool {
        Pool {
            idle: Mutex::new(HashMap::new()),
            timeout,
            moved: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    /// The failovers' cuts of `mark`'s bucket so far, sequentially
    /// consistent: what a stream reads after it marks itself one
    /// ([`Answer::into_stream`]), and a connect watches.
    fn moved(&self, mark: u64) -> u64 {
        self.moved[bucket(mark)].load(Ordering::SeqCst)
    }

    /// A pooled connection to `addr`, its word `mark`'s `WAITING` from here:
    /// a cut finds the request on it from the moment the pool's lock is let
    /// go. Written under the lock a cut takes to list the node's
    /// connections after it moves the count ([`Pool::cut`]), so a forward
    /// that reads the count after this ([`Pool::exchange`]) needs no fence
    /// of its own: the cut's lock came after this one, and finds `WAITING`,
    /// or before, and its count is read. The word stored and the count read
    /// sequentially consistent instead, the two were an `stlr` and an
    /// `ldar` on aarch64 -- the load waiting for the store to drain -- and
    /// an `xchg` on x86, every request.
    fn take(&self, addr: &str, mark: u64) -> Option<Idle> {
        let mut idle = self.idle.lock().unwrap_or_else(|e| e.into_inner());
        let i = idle.get_mut(addr).and_then(|k| k.idle.pop())?;
        i.conn.flight.word.store(mark | WAITING, Ordering::Relaxed);
        Some(i)
    }

    fn put(&self, addr: &str, s: Idle) {
        let mut idle = self.idle.lock().unwrap_or_else(|e| e.into_inner());
        let list = &mut idle.entry(addr.to_string()).or_default().idle;
        if list.len() < MAX_IDLE {
            list.push(s);
        }
    }

    fn connect(&self, addr: &str, within: Duration, write: Duration) -> io::Result<Conn> {
        dial(addr, within, write).map(|s| self.register(addr, s, 0))
    }

    /// [`Pool::connect`] of a forward, each address tried a
    /// [`CONNECT_LOOK`] at a time and given up on once a failover has moved
    /// a tenant of its bucket since the directory placed this one
    /// ([`Pool::moved`]), for the directory to be read again: a machine gone
    /// answers no connect, and a request dialing it as its tenant was failed
    /// over -- every one past the node's pooled connections, each held by a
    /// request waiting on it -- waited out the pool's timeout, where one out
    /// on a connection is cut ([`Pool::cut`]). A try is a fresh connect, its
    /// first SYN sent at once, as the system's own sends its next about a
    /// second after the first; a node a second away would never be reached,
    /// which no node a router forwards to is. A connect that fails outright
    /// -- refused, a process gone and its machine there -- is asked the same
    /// before it is the client's 502: refused as the failover came, it is
    /// sent where the directory places it now.
    fn connect_watching(
        &self,
        addr: &str,
        within: Duration,
        write: Duration,
        flying: Flying,
    ) -> Result<Conn, Failure> {
        let unsent = |e| Failure(Stage::Unsent, e);
        let moved = || self.moved(flying.mark) != flying.seen;
        let mut last = io::Error::new(io::ErrorKind::NotFound, format!("{addr} does not resolve"));
        let found = fenec_http::dial::resolve(addr, within);
        for a in found.map_err(|e| {
            if moved() {
                moved_meanwhile()
            } else {
                unsent(e)
            }
        })? {
            let deadline = Instant::now() + within;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                match TcpStream::connect_timeout(&a, left.min(CONNECT_LOOK)) {
                    Ok(s) => {
                        let s = ready(s, write).map_err(unsent)?;
                        return Ok(self.register(addr, s, flying.mark));
                    }
                    Err(_) if moved() => return Err(moved_meanwhile()),
                    Err(e) if timed_out(&e) => last = e,
                    Err(e) => {
                        last = e;
                        break;
                    }
                }
            }
        }
        Err(unsent(last))
    }

    /// `s`, a connection just made to `addr`, among the node's for a cut
    /// to find, the closed ones let go of: a lock a connection made. Its
    /// word is `mark`'s `WAITING` before it is listed, under the lock, as
    /// [`Pool::take`] writes a pooled one's.
    fn register(&self, addr: &str, s: TcpStream, mark: u64) -> Conn {
        let flight = Arc::new(Flight::of(&s, mark | WAITING));
        let mut idle = self.idle.lock().unwrap_or_else(|e| e.into_inner());
        let flights = &mut idle.entry(addr.to_string()).or_default().flights;
        flights.retain(|f| !f.closed());
        flights.push(Arc::clone(&flight));
        Conn { stream: s, flight }
    }

    /// Ends every request out to `addr` for `tenant` whose answer has not
    /// begun, and every stream of its, by shutting their connections down
    /// ([`Flight::cut`]): what a failover does for each tenant it moved off
    /// the node at `addr`, once the directory places it elsewhere -- the
    /// thread waiting on each then finds it so and answers its client by
    /// what the node may have run (`Router::after_cut`), where it waited
    /// for the timeout. A request whose answer's head is in is left to it:
    /// its answer is the node's. The connections are looked at with no
    /// lock held, a shutdown each.
    pub fn cut(&self, addr: &str, tenant: &str) -> Cuts {
        let mark = [mark_of(tenant)];
        self.move_marks(&mark);
        self.shut_marks(addr, &mark)
    }

    /// [`Pool::cut`] of every `(node's address, tenant)` of `failed` at once:
    /// what a router following the directory does with what one catch-up
    /// shows failed over, its directories' write locks held. Every bucket
    /// of theirs is moved first, once, and only then is a node's connection
    /// shut: moved a node at a time, a request cut off the first node read
    /// its count again, and the second node's move had the count it read
    /// move under it -- [`Ended::Moved`], and the cut was lost. Then one
    /// look at each node's connections for all of its tenants, each tested
    /// against their marks: a cut a tenant listed and copied the node's
    /// every connection a tenant, a failover of a whole node read at once
    /// its tenants times its connections under the lock. The nodes are
    /// found by a walk of the list each: a catch-up names one node, or a
    /// few.
    pub fn cut_all(&self, failed: &[(&str, &str)]) -> Cuts {
        let marks: Vec<u64> = failed.iter().map(|&(_, t)| mark_of(t)).collect();
        self.move_marks(&marks);
        let mut cuts = Cuts::default();
        let mut done: Vec<&str> = Vec::new();
        for &(addr, _) in failed {
            if done.contains(&addr) {
                continue;
            }
            done.push(addr);
            let mut theirs: Vec<u64> = failed
                .iter()
                .zip(&marks)
                .filter(|((a, _), _)| *a == addr)
                .map(|(_, &m)| m)
                .collect();
            theirs.sort_unstable();
            theirs.dedup();
            let shut = self.shut_marks(addr, &theirs);
            cuts.waiting += shut.waiting;
            cuts.streams += shut.streams;
        }
        cuts
    }

    /// Moves the bucket of each of `marks` once ([`Pool::moved`]): a
    /// forward of theirs that read the count before and goes out after
    /// finds it moved.
    fn move_marks(&self, marks: &[u64]) {
        const _: () = assert!(MOVED_BUCKETS <= 64, "the buckets moved are a word's bits");
        let mut bumped = 0u64;
        for &mark in marks {
            let b = bucket(mark);
            if bumped & (1 << b) == 0 {
                bumped |= 1 << b;
                self.moved[b].fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    /// Ends each connection to `addr` whose request or stream is one of the
    /// tenants' whose marks are `marks`, sorted, their buckets moved before
    /// ([`Pool::move_marks`]). A connection's word is read and then
    /// compared and changed ([`Flight::cut`]), so one taken for another
    /// tenant's request between the two is left: the count it read moved
    /// first.
    fn shut_marks(&self, addr: &str, marks: &[u64]) -> Cuts {
        #[cfg(test)]
        SHUTTING.with_borrow_mut(|told| {
            if let Some(told) = told {
                told(self, addr)
            }
        });
        let flights = {
            let mut idle = self.idle.lock().unwrap_or_else(|e| e.into_inner());
            match idle.get_mut(addr) {
                Some(k) => {
                    k.flights.retain(|f| !f.closed());
                    k.flights.clone()
                }
                None => Vec::new(),
            }
        };
        let mut cuts = Cuts::default();
        for f in flights {
            let mark = f.word.load(Ordering::SeqCst) & !STATE;
            if mark == 0 || marks.binary_search(&mark).is_err() {
                continue;
            }
            match f.cut(mark) {
                Some(WAITING) => cuts.waiting += 1,
                Some(_) => cuts.streams += 1,
                None => {}
            }
        }
        cuts
    }

    /// Sends a request and reads the response head. `headers` must not
    /// carry `Content-Length` or `Connection`; both are written here.
    pub fn send(
        &self,
        addr: &str,
        method: &str,
        target: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> io::Result<Answer> {
        let none = Flying::NONE;
        self.send_held(
            addr,
            method,
            target,
            headers,
            body,
            Duration::ZERO,
            None,
            none,
        )
    }

    /// [`Pool::send`] of a request the node may hold for `held` before it
    /// answers (`Fenec-Wait`): the answer is waited for that much longer.
    /// Cut at the timeout alone, a claim held longer was answered 502 here
    /// while the node went on to take a job for it, leased to no one.
    /// `client`, the socket of the request's own client, is watched while
    /// the head is waited for ([`answered_or_gone`]): its end is passed on
    /// to the node, and its reset lets go of the node's connection, the
    /// failure [`client_gone`]. `None` waits as an unheld request does,
    /// with not a system call more. `flying` is what a failover's cut knows
    /// the request by ([`Pool::cut`]), whose failure says so ([`ended_by`]).
    #[allow(clippy::too_many_arguments)]
    pub fn send_held(
        &self,
        addr: &str,
        method: &str,
        target: &str,
        headers: &[(&str, &str)],
        body: &[u8],
        held: Duration,
        client: Option<&TcpStream>,
        flying: Flying,
    ) -> io::Result<Answer> {
        let times = (self.timeout + held, self.timeout, self.timeout);
        self.send_staged(
            addr,
            method,
            target,
            headers,
            body,
            times,
            Replay::AsAsked,
            client,
            flying,
        )
        .map_err(|Failure(_, e)| e)
    }

    /// [`Pool::send_held`], its failure told by whether `addr` may have run
    /// the request ([`Tried`]): what a relay to a region's routers asks
    /// before it asks the next. A client gone may have had it run.
    #[allow(clippy::too_many_arguments)]
    pub fn send_tried(
        &self,
        addr: &str,
        method: &str,
        target: &str,
        headers: &[(&str, &str)],
        body: &[u8],
        held: Duration,
        client: Option<&TcpStream>,
    ) -> Result<Answer, Tried> {
        let times = (self.timeout + held, self.timeout, self.timeout);
        let replay = Replay::AsAsked;
        let none = Flying::NONE;
        match self.send_staged(
            addr, method, target, headers, body, times, replay, client, none,
        ) {
            Ok(a) => Ok(a),
            Err(Failure(Stage::Unsent, e)) => Err(Tried::Unrun(e)),
            Err(Failure(Stage::Unanswered, e)) if replay.allows(method, headers) => {
                Err(Tried::Unrun(e))
            }
            Err(Failure(_, e)) => Err(Tried::MayHaveRun(e)),
        }
    }

    /// [`Pool::send`] of a read its client marked (`Fenec-Read: replica`)
    /// to a read replica, connected, written and its answer's head read
    /// within `within` -- the wait its `Fenec-After` asks for and the
    /// router's `--replica-timeout` -- not the pool's timeout: a replica
    /// that does not answer by then is passed over for home, where the read
    /// runs as any. The write too: a body past the sockets' buffers, sent
    /// to a node that stopped reading, waited out the pool's. Sent again on a fresh connection where one ended before its
    /// answer, as [`replayable`] lets a marked read be. A failure says
    /// whether the replica took the whole request ([`Near`]).
    pub fn send_within(
        &self,
        addr: &str,
        method: &str,
        target: &str,
        headers: &[(&str, &str)],
        body: &[u8],
        within: Duration,
    ) -> Result<Answer, Near> {
        let times = (within, within, within);
        let replay = Replay::AsAsked;
        let none = Flying::NONE;
        match self.send_staged(
            addr, method, target, headers, body, times, replay, None, none,
        ) {
            Ok(a) => Ok(a),
            Err(Failure(Stage::Unsent | Stage::Unread, e)) => Err(Near::Unreached(e)),
            Err(Failure(Stage::Unanswered | Stage::Other | Stage::Abandoned | Stage::Cut, e)) => {
                Err(Near::Unanswered(e))
            }
        }
    }

    /// [`Pool::send`] of a read that must be answered by `deadline`: the
    /// connect and the head's reads are bounded by what is left of it
    /// (and by the pool's timeout), and [`Answer::read_body_by`] reads the
    /// body by it too. A read is harmless to run twice, so one whose
    /// connection ended before its answer goes again on a fresh one,
    /// whatever its method: a query across tenants is a `POST`.
    pub fn send_by(
        &self,
        addr: &str,
        target: &str,
        headers: &[(&str, &str)],
        body: &[u8],
        deadline: Instant,
    ) -> io::Result<Answer> {
        let left = left_until(deadline)?.min(self.timeout);
        self.send_timed(
            addr,
            "POST",
            target,
            headers,
            body,
            (left, left, self.timeout),
            Replay::Always,
        )
    }

    /// One request, its answer's head read within `times.0` a read, its
    /// connection made within `times.1` and each write of it bounded by
    /// `times.2`; `replay` is whether it may go again once sent whole (the
    /// module's comment).
    #[allow(clippy::too_many_arguments)]
    fn send_timed(
        &self,
        addr: &str,
        method: &str,
        target: &str,
        headers: &[(&str, &str)],
        body: &[u8],
        times: (Duration, Duration, Duration),
        replay: Replay,
    ) -> io::Result<Answer> {
        let none = Flying::NONE;
        self.send_staged(
            addr, method, target, headers, body, times, replay, None, none,
        )
        .map_err(|Failure(_, e)| e)
    }

    /// [`Self::send_timed`], its failure with how far it got, `client`
    /// watched while the head is waited for ([`Pool::send_held`]), `flying`
    /// what a failover's cut knows it by.
    #[allow(clippy::too_many_arguments)]
    fn send_staged(
        &self,
        addr: &str,
        method: &str,
        target: &str,
        headers: &[(&str, &str)],
        body: &[u8],
        times: (Duration, Duration, Duration),
        replay: Replay,
        client: Option<&TcpStream>,
        flying: Flying,
    ) -> Result<Answer, Failure> {
        let (read_timeout, connect_within, write_timeout) = times;
        let mut req = String::with_capacity(256);
        for part in [method, " ", target, " HTTP/1.1\r\nHost: ", addr, "\r\n"] {
            req.push_str(part);
        }
        for (k, v) in headers {
            for part in [k, ": ", v, "\r\n"] {
                req.push_str(part);
            }
        }
        // The id of the request this thread serves goes on to the node, which
        // takes it as its own: one request through the router, one id in
        // both logs. A forwarded request's own header is left out by
        // `forward`, so it is never sent twice.
        fenec_http::request_id::with(|id| {
            if !id.is_empty() {
                for part in [fenec_http::request_id::HEADER, ": ", id, "\r\n"] {
                    req.push_str(part);
                }
            }
        });
        // The trace this thread's request is in goes on as the id does,
        // its parent the span open now -- `forward`'s -- so the node's spans
        // hang under the router's. A forwarded request's own `traceparent`
        // is left out by `forward` while the router traces, and passed on
        // as it came while it does not.
        fenec_http::trace::outgoing(|parent, state| {
            for part in [fenec_http::trace::TRACEPARENT, ": ", parent, "\r\n"] {
                req.push_str(part);
            }
            if let Some(state) = state {
                for part in [fenec_http::trace::TRACESTATE, ": ", state, "\r\n"] {
                    req.push_str(part);
                }
            }
        });
        req.push_str(&format!(
            "Content-Length: {}\r\nConnection: keep-alive\r\n\r\n",
            body.len()
        ));
        let head = req.as_bytes();

        // The node closes its idle connections together, so a closed one
        // found here is let go of and the next one looked at.
        while let Some(idle) = self.take(addr, flying.mark) {
            if !open(&idle.conn.stream) {
                continue;
            }
            let set = (Some(idle.read_timeout), idle.write_timeout);
            let want = (read_timeout, write_timeout);
            match self.exchange(addr, idle.conn, set, (head, body), want, client, flying) {
                Ok(a) => return Ok(a),
                Err(Failure(Stage::Unsent, _)) => break,
                Err(Failure(Stage::Unanswered, _)) if replay.allows(method, headers) => break,
                Err(f) => return Err(f),
            }
        }
        let s = match flying.mark {
            0 => self
                .connect(addr, connect_within, write_timeout)
                .map_err(|e| Failure(Stage::Unsent, e))?,
            _ => self.connect_watching(addr, connect_within, write_timeout, flying)?,
        };
        let set = (None, write_timeout);
        let want = (read_timeout, write_timeout);
        self.exchange(addr, s, set, (head, body), want, client, flying)
    }

    /// One request, its head and body, and the head of its answer over
    /// `s`, whose read and write timeouts are `set` -- the read's where it
    /// is known -- each read of the answer bounded by `want.0` and each
    /// write by `want.1`. Each is set only where it is not already, a
    /// system call a request. `client` is watched until the answer begins
    /// ([`answered_or_gone`]). Out from its first byte to its answer's
    /// head, the request is what a failover's cut of `flying`'s tenant
    /// ends ([`Pool::cut`]).
    #[allow(clippy::too_many_arguments)]
    fn exchange(
        &self,
        addr: &str,
        s: Conn,
        set: (Option<Duration>, Duration),
        (head, body): (&[u8], &[u8]),
        want: (Duration, Duration),
        client: Option<&TcpStream>,
        flying: Flying,
    ) -> Result<Answer, Failure> {
        let (read_timeout, write_timeout) = want;
        // Out from here (`WAITING`, written as the pool handed it over): a
        // cut shutting it down now has macOS refuse the options (EINVAL),
        // which is the cut's to say.
        if set.0 != Some(read_timeout) {
            s.stream
                .set_read_timeout(Some(read_timeout))
                .map_err(|e| failed(&s.flight, Stage::Unsent, e))?;
        }
        if set.1 != write_timeout {
            s.stream
                .set_write_timeout(Some(write_timeout))
                .map_err(|e| failed(&s.flight, Stage::Unsent, e))?;
        }
        if flying.mark != 0
            && self.moved[bucket(flying.mark)].load(Ordering::Relaxed) != flying.seen
        {
            return Err(self.unsent_moved(addr, s, want, flying.mark));
        }
        let mut w = &s.stream;
        // Head and body in one `writev`. Written apart, a node that had
        // answered the connection as it took it (`fenec_http::accept`'s
        // 503) and closed it had Linux take the reset its closed socket sent
        // the head back inside the head's own `write`, and the body's met
        // EPIPE: unsent, its 503 unread, the write was told 502 (CI; in
        // Docker's Linux, three cores beside six busy loops, 809 and 853
        // puts through a router of 20 000, none since). A write that fails
        // leaves the request's last byte unsent: the call that would carry
        // it failed, or never came. Not that the failed call took none of
        // its bytes -- Linux takes none, but macOS's `dofilewrite` answers
        // EPIPE with -1 where part of the call went out. An empty body
        // writes nothing, and the head's last byte is then the last.
        let early = match fenec_http::http::write_parts(&mut w, head, body) {
            Ok(()) => None,
            // Timed out, the node is there and not reading: sent again, it
            // would wait as long.
            Err(e) if timed_out(&e) => return Err(failed(&s.flight, Stage::Unread, e)),
            // An answer in already on a connection made for this request
            // is the node's to a request it never read whole -- refused as
            // it took the connection, a body past its ceiling -- so it ran
            // nothing, and is the answer: a body past what one `writev`
            // takes met the reset after it. A pooled connection's byte may
            // be no answer of this request's (`open` looked before the
            // write), and its failure goes on to a fresh one.
            Err(e) if set.0.is_none() && answer_waiting(&s.stream) => Some(e),
            Err(e) => return Err(failed(&s.flight, Stage::Unsent, e)),
        };
        // The client's end, passed on as the node reads one from a client
        // of its own: its held claim sees it before its next run, and its
        // answer comes back as any. Shut for writing, the connection is
        // not kept for another request.
        let mut client_ended = false;
        if let (Some(client), None) = (client, &early) {
            match answered_or_gone(&s.stream, client, read_timeout) {
                Ok(Watched::Answering) => {}
                Ok(Watched::Ended) => {
                    let _ = s.stream.shutdown(std::net::Shutdown::Write);
                    client_ended = true;
                }
                Err(f @ Failure(Stage::Abandoned, _)) => return Err(f),
                Err(Failure(stage, e)) => return Err(failed(&s.flight, stage, e)),
            }
        }
        let kept_off = client_ended || early.is_some();
        match (self.head(addr, s, want, kept_off, flying), early) {
            (answer, None) => answer,
            // An answer in before the request went out whole is a fenec
            // node's refusal of one it never read whole -- 400, 411, 413,
            // 431, the 503 of a connection it served nothing on -- never a
            // 2xx or a 401: a node reads the whole body before it asks the
            // token or runs a byte. Under 400 it is a peer claiming what it
            // cannot have done, and the request stays unsent.
            (Ok(a), Some(_)) if a.status >= 400 => Ok(a),
            // A cut's shutdown met the head: the last byte never went out,
            // so it ran nothing, which a cut of a request sent whole would
            // deny -- an unkeyed write told 502 that could go to the new
            // primary.
            (Err(Failure(Stage::Cut, c)), Some(_)) => Err(Failure(Stage::Cut, unsent_cut(c))),
            // Nothing read that is an answer: the request is what it was.
            (_, Some(e)) => Err(Failure(Stage::Unsent, e)),
        }
    }

    /// The head of the answer to a request written over `s`, each read
    /// bounded by `want.0`; `kept_off`, the connection is not pooled after
    /// it.
    #[inline(always)]
    fn head(
        &self,
        addr: &str,
        s: Conn,
        (read_timeout, write_timeout): (Duration, Duration),
        kept_off: bool,
        flying: Flying,
    ) -> Result<Answer, Failure> {
        let mut reader = BufReader::new(s);
        let mut line = String::new();
        let read = reader.read_line(&mut line);
        let flight = &reader.get_ref().flight;
        match read {
            Ok(0) => {
                let eof = io::ErrorKind::UnexpectedEof.into();
                return Err(failed(flight, Stage::Unanswered, eof));
            }
            Ok(_) => {}
            // The connection's end, as each system reports it. Not any
            // error with nothing read: a line that is not UTF-8 is left
            // out of `line`, and that is an answer.
            Err(e) if line.is_empty() && ended(&e) => {
                return Err(failed(flight, Stage::Unanswered, e))
            }
            Err(e) => return Err(failed(flight, Stage::Other, e)),
        }
        // The head has begun, and the answer is the node's from here unless
        // a cut came first, whose shutdown may have cut it short.
        if !flight.answering(flying.mark) {
            let e = io::ErrorKind::UnexpectedEof.into();
            return Err(failed(flight, Stage::Unanswered, e));
        }
        let other = |e| Failure(Stage::Other, e);
        let status = line
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .ok_or_else(|| bad("malformed status line"))
            .map_err(other)?;
        let mut headers = Vec::new();
        let mut total = line.len();
        loop {
            line.clear();
            let n = reader.read_line(&mut line).map_err(other)?;
            total += n;
            if n == 0 {
                return Err(other(bad("truncated response head")));
            }
            if total > MAX_HEAD {
                return Err(other(bad("response head too large")));
            }
            let l = line.trim_end_matches(['\r', '\n']);
            if l.is_empty() {
                break;
            }
            if let Some((k, v)) = l.split_once(':') {
                headers.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
        let reusable = !kept_off
            && !headers.iter().any(|(k, v)| {
                k.eq_ignore_ascii_case("connection") && v.eq_ignore_ascii_case("close")
            });
        Ok(Answer {
            status,
            headers,
            reader: Some(reader),
            addr: addr.to_string(),
            reusable,
            read_timeout,
            write_timeout,
            flying,
        })
    }

    /// A forward a failover moved before a byte of it went out
    /// ([`Ended::Moved`]): its connection put back for another, where no cut
    /// has it, since its node may well be its tenant's still -- dropped, a
    /// failover's every straddling forward closed a healthy connection.
    #[cold]
    #[inline(never)]
    fn unsent_moved(&self, addr: &str, s: Conn, want: (Duration, Duration), mark: u64) -> Failure {
        if s.flight.rest(mark) {
            let (read_timeout, write_timeout) = want;
            let idle = Idle {
                conn: s,
                read_timeout,
                write_timeout,
            };
            self.put(addr, idle);
        }
        moved_meanwhile()
    }

    /// A whole request and its `Content-Length` body: the router's own
    /// calls to a node's `/_admin/`.
    pub fn call(
        &self,
        addr: &str,
        method: &str,
        target: &str,
        token: &str,
        body: &[u8],
    ) -> io::Result<(u16, Vec<u8>)> {
        self.call_with(addr, method, target, token, &[], body)
    }

    /// [`Pool::call`] with headers of its own besides the token.
    pub fn call_with(
        &self,
        addr: &str,
        method: &str,
        target: &str,
        token: &str,
        extra: &[(&str, &str)],
        body: &[u8],
    ) -> io::Result<(u16, Vec<u8>)> {
        let bearer = format!("Bearer {token}");
        let mut headers = vec![("Authorization", bearer.as_str())];
        headers.extend_from_slice(extra);
        let a = self.send(addr, method, target, &headers, body)?;
        let status = a.status;
        let body = a.read_body(self, method == "HEAD")?;
        Ok((status, body))
    }
}

/// Whether a request may go again once sent whole: always (a read across
/// tenants), or as [`replayable`] says of it -- asked where a send failed
/// alone: asked of every request, its scan of the headers was a cost of
/// each one through the router, and grew a comparison a header with the
/// read's mark.
#[derive(Clone, Copy)]
enum Replay {
    Always,
    AsAsked,
}

impl Replay {
    fn allows(self, method: &str, headers: &[(&str, &str)]) -> bool {
        match self {
            Replay::Always => true,
            Replay::AsAsked => replayable(method, headers),
        }
    }
}

/// Whether a request whose connection ended before its answer may be sent
/// again: one a node never writes for, or a write the node makes once
/// under its key, answering a second request with the first one's answer
/// -- or a request its client marked as a read (`Fenec-Read: replica`),
/// which every node runs read-only on every route, a `POST /query` among
/// them: the value [`fenec_http::read_marked`] takes, and no other, which
/// a node refuses unrun.
pub(crate) fn replayable(method: &str, headers: &[(&str, &str)]) -> bool {
    matches!(method, "GET" | "HEAD" | "OPTIONS")
        || headers.iter().any(|(k, v)| {
            k.eq_ignore_ascii_case("idempotency-key")
                || (k.eq_ignore_ascii_case(fenec_http::READ_HEADER) && fenec_http::marks_read(v))
        })
}

/// A connection to `addr`, its name looked up within `within` and each
/// address it stands for tried within `within`, its writes bounded by
/// `write`. The lookup is bounded as the connect is ([`fenec_http::dial`]):
/// left to the system's resolver, a node recorded by a name its resolver
/// was slow to refuse -- a container gone -- held every dial to it for the
/// resolver's 5 s and more, a lease grant among them, whatever the pool's
/// timeout said. Every dial goes through here: a forwarded request, a
/// relay to a region's router, a query across tenants, a read replica's
/// read and its upkeep, a health probe, a lease grant.
fn dial(addr: &str, within: Duration, write: Duration) -> io::Result<TcpStream> {
    let mut last = io::Error::new(io::ErrorKind::NotFound, format!("{addr} does not resolve"));
    for a in fenec_http::dial::resolve(addr, within)? {
        match TcpStream::connect_timeout(&a, within) {
            Ok(s) => return ready(s, write),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// How long a forward's connect waits on a try before it looks whether its
/// tenant was moved meanwhile ([`Pool::connect_watching`]).
const CONNECT_LOOK: Duration = Duration::from_secs(1);

/// `s`, just connected, as every request through the pool wants it.
fn ready(s: TcpStream, write: Duration) -> io::Result<TcpStream> {
    let _ = s.set_nodelay(true);
    // Once a connection: the same for every request but a read replica's,
    // which sets its own and is tracked with the connection
    // (`Idle::write_timeout`).
    s.set_write_timeout(Some(write))?;
    Ok(s)
}

fn timed_out(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

fn ended(e: &io::Error) -> bool {
    use io::ErrorKind::*;
    matches!(
        e.kind(),
        UnexpectedEof | ConnectionReset | ConnectionAborted | BrokenPipe | NotConnected
    )
}

/// Whether a pooled connection is still open: a read that does not wait
/// finds nothing to read. One the node closed reads its end, one it reset
/// an error, and one holding bytes no request asked for is no use either.
fn open(s: &TcpStream) -> bool {
    matches!(peek_now(s), Err(e) if e.kind() == io::ErrorKind::WouldBlock)
}

/// Whether bytes of an answer wait on `s`, read with no wait: after the
/// reset that failed a write, too, since macOS and Linux both hand a
/// socket's received bytes over before its error.
#[cold]
fn answer_waiting(s: &TcpStream) -> bool {
    matches!(peek_now(s), Ok(1..))
}

/// A `recv` of `MSG_PEEK | MSG_DONTWAIT`: one system call, where
/// `set_nonblocking`, `peek` and `set_nonblocking` again are three (0.31
/// us against 0.74 on an M1). The crate takes no `libc`.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn peek_now(s: &TcpStream) -> io::Result<usize> {
    use std::os::fd::AsRawFd;
    extern "C" {
        fn recv(fd: i32, buf: *mut u8, len: usize, flags: i32) -> isize;
    }
    const MSG_PEEK: i32 = 2;
    #[cfg(target_vendor = "apple")]
    const MSG_DONTWAIT: i32 = 0x80;
    #[cfg(not(target_vendor = "apple"))]
    const MSG_DONTWAIT: i32 = 0x40;
    let mut byte = 0u8;
    // SAFETY: one byte, into a byte that outlives the call.
    let n = unsafe { recv(s.as_raw_fd(), &mut byte, 1, MSG_PEEK | MSG_DONTWAIT) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn peek_now(s: &TcpStream) -> io::Result<usize> {
    s.set_nonblocking(true)?;
    let seen = s.peek(&mut [0u8]);
    s.set_nonblocking(false)?;
    seen
}

/// What became of a held request's client while its answer was waited for
/// ([`answered_or_gone`]).
#[derive(Debug, PartialEq, Eq)]
enum Watched {
    /// The node's socket moved first.
    Answering,
    /// The client's end came: closed, or half-closed and still reading,
    /// which read alike.
    Ended,
}

/// Told, on the thread cutting, each node's address as its connections
/// are about to be shut ([`Pool::shut_marks`]): what a test reads the
/// buckets' counts at then.
#[cfg(test)]
type Shutting = Option<Box<dyn FnMut(&Pool, &str)>>;
#[cfg(test)]
thread_local! {
    static SHUTTING: std::cell::RefCell<Shutting> = const { std::cell::RefCell::new(None) };
}

/// `poll`, declared by hand: the crate takes no `libc`.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
mod sys {
    #[repr(C)]
    pub(super) struct PollFd {
        pub(super) fd: i32,
        pub(super) events: i16,
        pub(super) revents: i16,
    }
    // `nfds_t`: an `unsigned int` on Apple's systems and in Android's
    // bionic, an `unsigned long` in glibc and musl.
    #[cfg(any(target_vendor = "apple", target_os = "android"))]
    pub(super) type Nfds = std::ffi::c_uint;
    #[cfg(all(target_os = "linux", not(target_vendor = "apple")))]
    pub(super) type Nfds = std::ffi::c_ulong;
    extern "C" {
        pub(super) fn poll(fds: *mut PollFd, n: Nfds, timeout: i32) -> i32;
    }
    // The same on both: `<sys/poll.h>`'s and `<asm-generic/poll.h>`'s.
    pub(super) const POLLIN: i16 = 0x1;
}

/// Waits up to `within` for the node's answer on `node` to begin, watching
/// `client` -- the held request's own client -- meanwhile: one `poll` of
/// both until either moves. The node's socket moving, by an answer, a
/// close or an error, is the head's read's to tell. The client's is looked
/// at as a pooled connection is ([`peek_now`]): its end is
/// [`Watched::Ended`], which the caller passes on to the node, and a reset
/// the client gone. Taken for gone, an end let go of the node's
/// connection, and a client that had half-closed after its request -- one
/// a node answers as any, its first run checking nothing -- was answered
/// nothing, its claim having taken a job on a busy queue all the same.
/// Bytes there are a next request a client sent ahead, which a client
/// that has gone sends none of: the client is let be, as the node lets a
/// client be whose next request it holds, and the node alone is waited
/// for. The `poll` is a `Fenec-Wait` request's alone: through the router a
/// claim answered at once took 0.5 us more at the median here (macOS), 0.7
/// on macOS and 1.2 on Linux in the review's measure, and one woken by a
/// job 8 us more, a `poll` and then the read where it was the read. A
/// watcher thread, prototyped in the review, was worse; the head read in
/// slices with the client looked at between was left on its reasoning:
/// every held claim -- a thousand of them -- woken each slice for nothing,
/// and a client's end seen a slice late.
///
/// An end cannot be told from a half-close -- a client that sent its
/// request and reads on -- so a client gone with its end, most timeouts and
/// aborts, is passed on as one: its claim leaves at the node's next run or
/// at its wait's end, as asked of the node itself, and its answer is
/// written to nobody and counted by its status. Taken for gone instead, a
/// half-closed client was answered nothing while its claim took a job; a
/// reset alone says the client can read nothing more.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn answered_or_gone(
    node: &TcpStream,
    client: &TcpStream,
    within: Duration,
) -> Result<Watched, Failure> {
    use std::os::fd::AsRawFd;
    use sys::{poll, PollFd, POLLIN};
    let deadline = Instant::now() + within;
    let mut watched = 2;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            let e = io::Error::new(io::ErrorKind::TimedOut, "the node did not answer in time");
            return Err(Failure(Stage::Other, e));
        }
        // Rounded up: a wait of what rounds down to 0 ms would spin.
        let ms = left.as_nanos().div_ceil(1_000_000).min(i32::MAX as u128) as i32;
        let mut fds = [
            PollFd {
                fd: node.as_raw_fd(),
                events: POLLIN,
                revents: 0,
            },
            PollFd {
                fd: client.as_raw_fd(),
                events: POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: `watched` of the two entries, which outlive the call.
        let n = unsafe { poll(fds.as_mut_ptr(), watched, ms) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            // The wait is the head's read's, as it was before the watch.
            return Ok(Watched::Answering);
        }
        if fds[0].revents != 0 {
            return Ok(Watched::Answering);
        }
        if watched == 2 && fds[1].revents != 0 {
            match peek_now(client) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Ok(1..) => watched = 1,
                Ok(0) => return Ok(Watched::Ended),
                Err(_) => return Err(Failure(Stage::Abandoned, io::Error::other(ClientGone))),
            }
        }
    }
}

/// No `poll` declared here: the head is waited for as an unheld request's.
#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn answered_or_gone(_: &TcpStream, _: &TcpStream, _: Duration) -> Result<Watched, Failure> {
    Ok(Watched::Answering)
}

/// What became of a client while its request waited out a pause
/// ([`pause_watching`]).
#[derive(Debug, PartialEq, Eq)]
pub enum Paused {
    /// The pause ran out, the client there -- or not watched.
    Here,
    /// The client's end came: closed, or half-closed and still reading.
    Ended,
    /// The client reset its connection: nobody is left to answer.
    Gone,
}

/// Waits `pause`, `client` -- the request's own client, where the router
/// can look at it -- watched meanwhile as a held request's is
/// ([`answered_or_gone`]): one `poll`, the pause ending early at the
/// client's end or reset. What a following router's request asks a copy
/// again after (`Router::copy_pause`): re-sent past its client's leaving,
/// a request whose copy had no grant yet was sent every pause for as long
/// as the pauses ran. Bytes of a next request a client sent ahead say it is
/// there, and the rest of the pause is waited; so is it where the `poll`
/// fails other than interrupted, as [`answered_or_gone`] stops watching:
/// asked again at once, a `poll` that kept failing spun for the pause.
pub fn pause_watching(client: Option<&TcpStream>, pause: Duration) -> Paused {
    let Some(client) = client else {
        std::thread::sleep(pause);
        return Paused::Here;
    };
    let deadline = Instant::now() + pause;
    let rest = || std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Paused::Here;
        }
        match readable(client, left) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => {
                rest();
                return Paused::Here;
            }
        }
        match peek_now(client) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Ok(1..) => {
                rest();
                return Paused::Here;
            }
            Ok(0) => return Paused::Ended,
            Err(_) => return Paused::Gone,
        }
    }
}

/// Whether `s` has something to read -- bytes, its end, its reset --
/// within `within`: `Ok(false)` past it, the `poll`'s own failure an error.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn readable(s: &TcpStream, within: Duration) -> io::Result<bool> {
    use std::os::fd::AsRawFd;
    use sys::{poll, PollFd, POLLIN};
    // Rounded up: a wait of what rounds down to 0 ms would spin.
    let ms = within.as_nanos().div_ceil(1_000_000).min(i32::MAX as u128) as i32;
    let mut fd = PollFd {
        fd: s.as_raw_fd(),
        events: POLLIN,
        revents: 0,
    };
    // SAFETY: one entry, which outlives the call.
    match unsafe { poll(&mut fd, 1, ms) } {
        n if n < 0 => Err(io::Error::last_os_error()),
        n => Ok(n > 0 && fd.revents != 0),
    }
}

/// No `poll` declared here: the pause is slept whole, the client looked at
/// once after it.
#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn readable(_: &TcpStream, within: Duration) -> io::Result<bool> {
    std::thread::sleep(within);
    Ok(true)
}

impl Answer {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Whether a router asked answered as it took the connection, before
    /// it read a byte of the request (`fenec_http::accept::UNSERVED_HEADER`):
    /// it ran nothing, and another router of its region may be asked.
    pub fn unserved(&self) -> bool {
        self.status == 503 && self.header(fenec_http::accept::UNSERVED_HEADER).is_some()
    }

    /// `None` means the body runs to the close: a stream.
    pub fn content_length(&self) -> Option<usize> {
        self.header("content-length")?.parse().ok()
    }

    /// [`Answer::read_body`], the answer kept for its head: the connection
    /// itself handed back to the pool, read to its end, and the answer left
    /// with none. What reads a read replica's whole answer before a byte of
    /// it goes to the client, so that one broken off goes home instead. It
    /// kept the reader and pooled a copy of its descriptor, as `dup` and a
    /// `close` more every read a replica answered.
    pub fn read_body_kept(&mut self, pool: &Pool, head_only: bool) -> io::Result<Vec<u8>> {
        let Some(len) = self.content_length() else {
            return Err(bad("a stream is read as it comes"));
        };
        let Some(mut reader) = self.reader.take() else {
            return Err(read_already());
        };
        let len = if head_only { 0 } else { len };
        let body = body_of(&mut reader, len, |_| Ok(()))?;
        if self.reusable && reader.buffer().is_empty() {
            let idle = Idle {
                conn: reader.into_inner(),
                read_timeout: self.read_timeout,
                write_timeout: self.write_timeout,
            };
            pool.put(&self.addr, idle);
        }
        self.reusable = false;
        Ok(body)
    }

    /// Reads a `Content-Length` body and hands the connection back to the
    /// pool when the node keeps it open. An answer with no length is
    /// refused at its head, as [`Answer::read_body_kept`] and
    /// [`Answer::read_body_by`] refuse one, and its connection let go of.
    /// Read to its close, it grew the router's memory for as long as its
    /// peer sent: what reads a body here is the router's own call to a
    /// node's `/_admin/` ([`Pool::call`]) or to a region's router's
    /// `/_shard/`, and a peer -- buggy, or not the router's -- answering one
    /// in chunks or with a body that never ends held the calling thread and
    /// took the memory every tenant's requests needed. Every answer those
    /// peers write carries its length (fenec-http's `Response::write`, a
    /// tenant's image among them) and none of these calls asks for a
    /// stream, while a stream a client asked for goes out as it comes
    /// (`Router::answer_out`, which reads a body here only where it has a
    /// length). Refused rather than capped: a cap would be a bound picked
    /// for an answer that never comes, and reading up to it held the
    /// thread as long as the peer chose to trickle it in.
    pub fn read_body(self, pool: &Pool, head_only: bool) -> io::Result<Vec<u8>> {
        let Some(len) = self.content_length() else {
            return Err(bad(
                "an answer with no length, which only a stream sends and this call asks for none",
            ));
        };
        let Some(mut reader) = self.reader else {
            return Err(read_already());
        };
        let len = if head_only { 0 } else { len };
        let body = body_of(&mut reader, len, |_| Ok(()))?;
        // Bytes left in the buffer would be the start of an answer nobody
        // asked for; such a connection is not reused.
        if self.reusable && reader.buffer().is_empty() {
            let idle = Idle {
                conn: reader.into_inner(),
                read_timeout: self.read_timeout,
                write_timeout: self.write_timeout,
            };
            pool.put(&self.addr, idle);
        }
        Ok(body)
    }

    /// [`Answer::read_body`] of an answer that must be in by `deadline`:
    /// each read waits for what is left of it, so a node that sends its
    /// body a byte at a time cannot stretch the wait past it. The read
    /// timeout is set only where the buffer is empty -- a small answer
    /// comes in with its head, in the one read that took that.
    pub fn read_body_by(mut self, pool: &Pool, deadline: Instant) -> io::Result<Vec<u8>> {
        let Some(len) = self.content_length() else {
            return Err(bad("an answer with no length: a stream"));
        };
        let Some(mut reader) = self.reader else {
            return Err(read_already());
        };
        let timeout = &mut self.read_timeout;
        let body = body_of(&mut reader, len, |reader| {
            if reader.buffer().is_empty() {
                let left = left_until(deadline)?.min(*timeout);
                reader.get_ref().stream.set_read_timeout(Some(left))?;
                *timeout = left;
            }
            Ok(())
        })?;
        if self.reusable && reader.buffer().is_empty() {
            let idle = Idle {
                conn: reader.into_inner(),
                read_timeout: self.read_timeout,
                write_timeout: self.write_timeout,
            };
            pool.put(&self.addr, idle);
        }
        Ok(body)
    }

    /// The rest of the answer as a byte stream, with no read timeout: an SSE
    /// stream is silent until something changes (the node's keep-alive
    /// line bounds the silence). A failover's cut ends it from here
    /// ([`Pool::cut`]): a node gone silent sends no keep-alive and no end,
    /// and its stream was copied for good, its client never opening it
    /// again where its tenant had gone. A cut while its head came in, which
    /// passed the answer over as begun, moved its tenant's bucket in `pool`:
    /// asked as the stream is marked one, sequentially consistent with the
    /// cut's look ([`Flight::streaming`]), such a stream ends here, an
    /// error, its client opening it again where the directory places it.
    /// Asked at the head instead, every answer of every forward read its
    /// headers for a length once more, the router's added p50 some 0.3 us.
    pub fn into_stream(self, pool: &Pool) -> io::Result<impl Read> {
        let Some(reader) = self.reader else {
            return Err(read_already());
        };
        let conn = reader.get_ref();
        conn.flight.streaming();
        let Flying { mark, seen } = self.flying;
        if mark != 0 && pool.moved(mark) != seen {
            return Err(io::Error::other(Ended::Cut { whole: true }));
        }
        conn.stream.set_read_timeout(None)?;
        Ok(reader)
    }
}

/// Whether the node at `addr` answers `GET /_health` within `within`, on a
/// connection of its own, made for the question and closed after, as a
/// client new to the node reaches it. Any status is an answer: a node
/// answers it on the connection's own thread before any token, lock or
/// routing, so one that answers at all takes connections and runs them
/// ([`crate::readers::HEALTH_WAIT`]). Asked on a thread of its own, so the
/// question, the name's lookup within `within` among it, holds up no
/// request.
pub fn answers_health(addr: &str, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    let Ok(mut s) = dial(addr, within, within) else {
        return false;
    };
    let asked = format!("GET /_health HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    if s.write_all(asked.as_bytes()).is_err() {
        return false;
    }
    // An answer's first bytes, each read bounded by what is left.
    let mut head = [0u8; 5];
    let mut got = 0;
    while got < head.len() {
        let Ok(left) = left_until(deadline) else {
            return false;
        };
        if s.set_read_timeout(Some(left)).is_err() {
            return false;
        }
        match s.read(&mut head[got..]) {
            Ok(0) => return false,
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
    }
    &head == b"HTTP/"
}

/// The `len` bytes of a body off `reader`, `before` run ahead of each read,
/// read into a buffer of the whole length ([`zeroed`]).
fn body_of(
    reader: &mut BufReader<Conn>,
    len: usize,
    mut before: impl FnMut(&BufReader<Conn>) -> io::Result<()>,
) -> io::Result<Vec<u8>> {
    let mut body = zeroed(len)?;
    let mut got = 0;
    while got < len {
        before(reader)?;
        match reader.read(&mut body[got..]) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(body)
}

/// `len` zero bytes for a body the node says is that long: the allocation
/// `vec![0u8; len]` makes -- `alloc_zeroed`, the system's zero pages, each
/// taken up as it is first written -- with its failure an error, a 502,
/// where `vec!`'s aborts. A node, buggy or not the router's, declaring a
/// petabyte and sending three bytes aborted the router, every tenant's
/// clients with it. A buffer grown as the bytes came instead, from a
/// mebibyte fourfold, copied what came at each growth -- a body of 4 MiB
/// and a byte 596 us against 440 in the review's measure, every forwarded
/// answer past a mebibyte slower -- and a node sending without end under
/// such a length met a growth whose allocation aborted all the same; a
/// first buffer of 64 MiB was as fast, past which the growth was the same.
/// So the length is believed as far as the system lends it, which costs
/// what `vec!` did at every size measured, 200 B to 64 MiB, and a bound on
/// the length would refuse a tenant's image on its way through a move.
/// What the system lends is the system's: macOS refuses a petabyte, and
/// the refusal is the 502; Linux, overcommitting, lends it -- in the
/// review's Docker run two pages of it were taken up -- and the 502 is the
/// answer broken off at its three bytes. Either way a lying length costs
/// the pages its bytes reach, and a node that keeps sending grows the
/// router's memory as any large answer does, which was so before.
fn zeroed(len: usize) -> io::Result<Vec<u8>> {
    if len == 0 {
        return Ok(Vec::new());
    }
    let refused = || {
        io::Error::new(
            io::ErrorKind::OutOfMemory,
            format!("the node's answer says it is {len} bytes, which this router cannot hold"),
        )
    };
    let layout = std::alloc::Layout::array::<u8>(len).map_err(|_| refused())?;
    // SAFETY: a layout of a non-zero size.
    let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
    if ptr.is_null() {
        return Err(refused());
    }
    // SAFETY: allocated by the global allocator for exactly `len` bytes of
    // `u8`'s alignment, every one of them initialised to zero -- the
    // allocation a `Vec<u8>` of that capacity owns, as `vec!` makes it.
    Ok(unsafe { Vec::from_raw_parts(ptr, len, len) })
}

/// What is left until `deadline`, or the timeout it has come to.
fn left_until(deadline: Instant) -> io::Result<Duration> {
    match deadline.checked_duration_since(Instant::now()) {
        Some(left) if !left.is_zero() => Ok(left),
        _ => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the query's time ran out",
        )),
    }
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

#[cfg(test)]
impl Pool {
    /// The bound a call through the pool is held to, for a test to ask.
    pub(crate) fn timeout(&self) -> Duration {
        self.timeout
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// `s` as a connection the pool did not make: no cut finds it.
    fn unpooled(s: TcpStream) -> Conn {
        let flight = Arc::new(Flight::of(&s, WAITING));
        Conn { flight, stream: s }
    }

    /// A connection, and the node's end of it.
    fn pair() -> (TcpStream, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let ours = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (theirs, _) = l.accept().unwrap();
        (ours, theirs)
    }

    /// Waits until a read that does not wait on `s` stops finding nothing:
    /// what the node's end did has arrived.
    fn arrived(s: &TcpStream) -> io::Result<usize> {
        let until = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            match peek_now(s) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    assert!(std::time::Instant::now() < until, "nothing arrived");
                    std::thread::yield_now();
                }
                seen => return seen,
            }
        }
    }

    /// A connection the node's end reset, once the reset has arrived:
    /// closed with a byte it never read, a socket resets its connection.
    fn reset() -> TcpStream {
        let (mut ours, theirs) = pair();
        ours.write_all(b"x").unwrap();
        // The byte is there before the close, or the close would be a FIN.
        theirs
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        assert_eq!(theirs.peek(&mut [0u8]).unwrap(), 1);
        drop(theirs);
        let reset = arrived(&ours).unwrap_err();
        assert_eq!(reset.kind(), io::ErrorKind::ConnectionReset, "{reset}");
        ours
    }

    #[test]
    fn the_look_tells_an_open_connection_from_a_closed_or_reset_one() {
        let (ours, theirs) = pair();
        assert!(open(&ours));
        drop(theirs);
        assert_eq!(arrived(&ours).unwrap(), 0);
        assert!(!open(&ours));

        assert!(!open(&reset()));

        // Bytes no request asked for.
        let (ours, mut theirs) = pair();
        theirs.write_all(b"HTTP/1.1 200 OK\r\n").unwrap();
        assert_eq!(arrived(&ours).unwrap(), 1);
        assert!(!open(&ours));
    }

    /// How far a request of one write got over `s`. Of two, the node's
    /// end could answer the first with its reset before the second, which
    /// the request not wholly sent rightly makes unsent.
    fn stage(pool: &Pool, s: TcpStream, set: Option<Duration>) -> &'static str {
        let head = b"DELETE /t/acme/notes/1 HTTP/1.1\r\nContent-Length: 0\r\n\r\n";
        let (both, want) = ((set, pool.timeout), (pool.timeout, pool.timeout));
        match pool.exchange(
            "node",
            unpooled(s),
            both,
            (head, b""),
            want,
            None,
            Flying::NONE,
        ) {
            Ok(_) => "answered",
            Err(Failure(Stage::Unsent, _)) => "unsent",
            Err(Failure(Stage::Unanswered, _)) => "unanswered",
            Err(Failure(Stage::Unread | Stage::Other | Stage::Abandoned | Stage::Cut, e)) => {
                panic!("{e}")
            }
        }
    }

    /// What the look cannot see -- a close on its way -- is staged by how
    /// far the request got, whatever each system calls the failure.
    #[test]
    fn a_failed_exchange_is_staged_by_how_far_the_request_got() {
        let pool = Pool::new(Duration::from_secs(10));
        // Reset: nothing goes out. macOS refuses the read timeout's
        // `setsockopt` (EINVAL) and the write (EPIPE); Linux takes the
        // first and refuses the write (EPIPE, or the reset itself).
        assert_eq!(stage(&pool, reset(), None), "unsent");
        assert_eq!(stage(&pool, reset(), Some(pool.timeout)), "unsent");
        // Closed: the system takes the request, and the answer is the
        // connection's end, or the reset the node's end answered it with.
        let (ours, theirs) = pair();
        drop(theirs);
        assert_eq!(arrived(&ours).unwrap(), 0);
        assert_eq!(stage(&pool, ours, Some(pool.timeout)), "unanswered");
    }

    /// What a held request's wait makes of its client: its end -- closed or
    /// half-closed, which read alike -- is passed on, its reset is the
    /// client gone, bytes it sends ahead are neither, and the node's answer
    /// ends the wait whatever the client did.
    #[test]
    fn a_held_request_s_wait_tells_a_client_s_end_from_its_reset_and_bytes_sent_ahead() {
        let long = Duration::from_secs(30);
        // The client's end, closed and half-closed.
        let (node, _node_end) = pair();
        let (client, client_end) = pair();
        drop(client_end);
        assert_eq!(
            answered_or_gone(&node, &client, long).ok(),
            Some(Watched::Ended)
        );
        let (node, _node_end) = pair();
        let (client, client_end) = pair();
        client_end.shutdown(std::net::Shutdown::Write).unwrap();
        assert_eq!(
            answered_or_gone(&node, &client, long).ok(),
            Some(Watched::Ended)
        );
        // Its reset, left for the wait to find: Linux reports a reset to
        // the first read that meets it and an end to every one after, so a
        // reset looked at once already reads as an end.
        let (node, _node_end) = pair();
        let (mut client, client_end) = pair();
        client.write_all(b"x").unwrap();
        client_end
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        assert_eq!(client_end.peek(&mut [0u8]).unwrap(), 1);
        drop(client_end);
        let gone = |r: Result<Watched, Failure>| matches!(r, Err(Failure(Stage::Abandoned, _)));
        assert!(gone(answered_or_gone(&node, &client, long)));
        // Bytes of a next request, then its end: whichever the wait sees
        // first, the bytes come before the end, and the node alone is
        // waited for -- to the wait's end, a timeout, as with nothing from
        // either.
        let short = Duration::from_millis(50);
        let timed = |r: Result<Watched, Failure>| matches!(&r, Err(Failure(Stage::Other, e)) if timed_out(e));
        let (node, _node_end) = pair();
        let (client, mut client_end) = pair();
        client_end
            .write_all(b"GET /t/acme/notes HTTP/1.1\r\n")
            .unwrap();
        drop(client_end);
        assert!(timed(answered_or_gone(&node, &client, short)));
        let (node, _node_end) = pair();
        let (client, _client_end) = pair();
        assert!(timed(answered_or_gone(&node, &client, short)));
        // The node's answer ends the wait, its client gone or not.
        let (node, mut node_end) = pair();
        let (client, client_end) = pair();
        node_end.write_all(b"HTTP/1.1 200 OK\r\n").unwrap();
        assert_eq!(arrived(&node).unwrap(), 1);
        drop(client_end);
        assert_eq!(
            answered_or_gone(&node, &client, long).ok(),
            Some(Watched::Answering)
        );
    }

    /// A pause watching its client ends early at the client's end, closed
    /// or half-closed, and at its reset, and runs out with the client
    /// there, bytes of a next request sent ahead among them, or not
    /// watched.
    #[test]
    fn a_pause_watching_its_client_ends_at_its_end_or_reset() {
        let long = Duration::from_secs(30);
        let (client, client_end) = pair();
        drop(client_end);
        assert_eq!(pause_watching(Some(&client), long), Paused::Ended);
        let (client, client_end) = pair();
        client_end.shutdown(std::net::Shutdown::Write).unwrap();
        assert_eq!(pause_watching(Some(&client), long), Paused::Ended);
        let (mut client, client_end) = pair();
        client.write_all(b"x").unwrap();
        client_end
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        assert_eq!(client_end.peek(&mut [0u8]).unwrap(), 1);
        drop(client_end);
        assert_eq!(pause_watching(Some(&client), long), Paused::Gone);
        let short = Duration::from_millis(50);
        let (client, _client_end) = pair();
        let at = Instant::now();
        assert_eq!(pause_watching(Some(&client), short), Paused::Here);
        assert!(at.elapsed() >= short);
        let (client, mut client_end) = pair();
        client_end.write_all(b"GET / HTTP/1.1\r\n").unwrap();
        let at = Instant::now();
        assert_eq!(pause_watching(Some(&client), short), Paused::Here);
        assert!(at.elapsed() >= short);
        let at = Instant::now();
        assert_eq!(pause_watching(None, short), Paused::Here);
        assert!(at.elapsed() >= short);
    }

    /// An answer to one request, its head and `body`, from a node's end
    /// that reads the request first and closes after: the head read.
    fn answered(pool: &Pool, length: usize, body: Vec<u8>) -> Answer {
        let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {length}\r\n\r\n");
        answered_with(pool, head, move |mut theirs| {
            let _ = theirs.write_all(&body);
        })
    }

    /// An answer to one request from a node's end that reads the request
    /// first, writes `head` and has `rest` write what follows it: the head
    /// read.
    fn answered_with(
        pool: &Pool,
        head: String,
        rest: impl FnOnce(TcpStream) + Send + 'static,
    ) -> Answer {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let ours = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (mut theirs, _) = l.accept().unwrap();
        std::thread::spawn(move || {
            let mut asked = Vec::new();
            let mut chunk = [0u8; 1024];
            while !asked.windows(4).any(|w| w == b"\r\n\r\n") {
                match theirs.read(&mut chunk) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => asked.extend_from_slice(&chunk[..n]),
                }
            }
            if theirs.write_all(head.as_bytes()).is_ok() {
                rest(theirs);
            }
        });
        let head = b"GET /t/acme/notes HTTP/1.1\r\nContent-Length: 0\r\n\r\n";
        let (set, want) = ((None, pool.timeout), (pool.timeout, pool.timeout));
        match pool.exchange(
            "node",
            unpooled(ours),
            set,
            (head, b""),
            want,
            None,
            Flying::NONE,
        ) {
            Ok(a) => a,
            Err(Failure(_, e)) => panic!("{e}"),
        }
    }

    /// A body's buffer is the length its node says, with the allocation's
    /// failure an error: a node declaring a petabyte and sending three
    /// bytes is an error to each of the three readers -- the allocation
    /// refused, or lent and the answer broken off -- where `vec!`'s failure
    /// aborted the router; past what an allocation can be, it is refused
    /// before it is asked; and a body of 20 MiB comes whole.
    #[test]
    fn a_body_s_buffer_failing_to_allocate_is_an_error_not_an_abort() {
        let pool = Pool::new(Duration::from_secs(10));
        let lie = 1usize << 50;
        let failed = |r: io::Result<Vec<u8>>| matches!(r, Err(e) if matches!(e.kind(), io::ErrorKind::OutOfMemory | io::ErrorKind::UnexpectedEof));
        assert!(failed(
            answered(&pool, lie, b"abc".to_vec()).read_body(&pool, false)
        ));
        assert!(failed(
            answered(&pool, lie, b"abc".to_vec()).read_body_kept(&pool, false)
        ));
        let by = Instant::now() + Duration::from_secs(10);
        assert!(failed(
            answered(&pool, lie, b"abc".to_vec()).read_body_by(&pool, by)
        ));
        let past = zeroed(isize::MAX as usize + 1).unwrap_err();
        assert_eq!(past.kind(), io::ErrorKind::OutOfMemory, "{past}");
        assert!(zeroed(0).unwrap().is_empty());
        let whole: Vec<u8> = (0..(20 << 20) + 7).map(|i| (i % 251) as u8).collect();
        let n = whole.len();
        let body = answered(&pool, n, whole.clone())
            .read_body(&pool, false)
            .unwrap();
        assert!(body == whole);
        let body = answered(&pool, n, whole.clone())
            .read_body_kept(&pool, false)
            .unwrap();
        assert!(body == whole);
        let body = answered(&pool, n, whole.clone())
            .read_body_by(&pool, by)
            .unwrap();
        assert!(body == whole);
    }

    /// An answer with no length is refused at its head by each of the
    /// three readers, and its connection let go of: a node sending a body
    /// with no length and no end is cut off long before it is done, where
    /// `read_body` read it to its close -- the router's memory growing for
    /// as long as the node sent, the calling thread held all the while.
    #[test]
    fn an_answer_with_no_length_is_refused_at_its_head() {
        const SENDS: usize = 64 << 20;
        type Reader = fn(Answer, &Pool, Instant) -> io::Result<Vec<u8>>;
        let readers: [Reader; 3] = [
            |a, pool, _| a.read_body(pool, false),
            |mut a, pool, _| a.read_body_kept(pool, false),
            |a, pool, by| a.read_body_by(pool, by),
        ];
        let pool = Pool::new(Duration::from_secs(10));
        for read in readers {
            let (told, sent) = std::sync::mpsc::channel();
            let head = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n".to_string();
            let a = answered_with(&pool, head, move |mut theirs| {
                let chunk = [b'x'; 64 << 10];
                let mut n = 0;
                while n < SENDS && theirs.write_all(&chunk).is_ok() {
                    n += chunk.len();
                }
                let _ = told.send(n);
            });
            let by = Instant::now() + Duration::from_secs(10);
            let e = read(a, &pool, by).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
            // The node's writes fail once the connection is let go of,
            // past what the two ends' buffers hold and short of its end.
            let n = sent.recv_timeout(Duration::from_secs(30)).unwrap();
            assert!(n < SENDS, "the node sent its {n} bytes whole");
        }
    }

    const ASKED: &[u8] = b"GET /t/acme/notes HTTP/1.1\r\nContent-Length: 0\r\n\r\n";

    /// Reads one request off the node's end, its head to its blank line.
    fn request_on(theirs: &mut TcpStream) {
        let mut asked = Vec::new();
        let mut chunk = [0u8; 1024];
        // Bounded: a request that never goes is the failure, not a hang.
        theirs
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        while !asked.windows(4).any(|w| w == b"\r\n\r\n") {
            match theirs.read(&mut chunk) {
                Ok(0) | Err(_) => panic!("no request came"),
                Ok(n) => asked.extend_from_slice(&chunk[..n]),
            }
        }
    }

    /// What the one connection the pool knows to `addr` that is not closed
    /// is doing.
    fn state_of(pool: &Pool, addr: &str) -> u64 {
        let kept = pool.idle.lock().unwrap();
        kept.get(addr)
            .and_then(|k| k.flights.iter().find(|f| !f.closed()))
            .map_or(CLOSED, |f| f.word.load(Ordering::SeqCst) & STATE)
    }

    /// `ASKED` sent over a connection the pool knows to `n1`, as `flying`:
    /// its node's end, and the exchange running on a thread of its own.
    fn out<'s>(
        s: &'s std::thread::Scope<'s, '_>,
        pool: &'s Pool,
        flying: Flying,
    ) -> (
        TcpStream,
        std::thread::ScopedJoinHandle<'s, Result<Answer, Failure>>,
    ) {
        let (ours, theirs) = pair();
        let conn = pool.register("n1", ours, flying.mark);
        let want = (pool.timeout, pool.timeout);
        let set = (None, pool.timeout);
        let exchange =
            s.spawn(move || pool.exchange("n1", conn, set, (ASKED, b""), want, None, flying));
        (theirs, exchange)
    }

    /// What `e` says a failover did, where the exchange failed.
    fn ended(r: Result<Answer, Failure>) -> Ended {
        match r {
            Err(Failure(Stage::Cut, e)) => ended_by(&e).expect("a failover's"),
            Err(Failure(_, e)) => panic!("not a failover's: {e}"),
            Ok(a) => panic!("answered {}", a.status),
        }
    }

    /// A tenant other than `of` whose mark is in its bucket, or in another.
    fn bucketed(of: &str, same: bool) -> String {
        let b = bucket(mark_of(of));
        (0..100_000)
            .map(|i| format!("tenant-{i}"))
            .find(|t| t != of && (bucket(mark_of(t)) == b) == same)
            .expect("one in 64 names is in a bucket")
    }

    /// Whether the node's end of a connection has heard nothing, its end
    /// included: the router kept it.
    fn quiet(theirs: &TcpStream) -> bool {
        matches!(peek_now(theirs), Err(e) if e.kind() == io::ErrorKind::WouldBlock)
    }

    /// No two tenant names share a mark: every name of 8 bytes made of
    /// `shop-` and three of `[a-z0-9_-]`, every `tenant-` and one printable
    /// byte, and every 16 bytes made of a fixed 13 and three -- each last
    /// byte's top bits among what differs, which the mark kept low dropped:
    /// `tenant-1` and `tenant-q` shared one, and a failover of one cut the
    /// other's write on a node that still served it.
    #[test]
    fn tenant_names_differing_anywhere_do_not_share_a_mark() {
        let alpha = b"abcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut seen = std::collections::HashMap::new();
        let mut names = Vec::new();
        for a in alpha {
            for b in alpha {
                for c in alpha {
                    let tail = String::from_utf8(vec![*a, *b, *c]).unwrap();
                    names.push(format!("shop-{tail}"));
                    names.push(format!("customer-regi{tail}"));
                }
            }
        }
        for byte in 0x21u8..0x7f {
            names.push(format!("tenant-{}", byte as char));
        }
        for name in &names {
            let mark = mark_of(name);
            assert_eq!(mark & STATE, 0, "{name}");
            assert_ne!(mark, 0, "{name}");
            if let Some(other) = seen.insert(mark, name) {
                panic!("{name} and {other} share a mark");
            }
        }
        for (a, b) in [("tenant-1", "tenant-q"), ("demo-app", "demo-ap0")] {
            assert_ne!(mark_of(a), mark_of(b), "{a} {b}");
        }
    }

    /// A request out to a node whose answer has not begun is what a cut of
    /// its tenant ends: its exchange is told it was cut, its request having
    /// gone out whole, and the node's end reads the connection's end. A cut
    /// of another tenant on the node leaves it be. A stream handed over to
    /// be copied is ended the same way, its reader meeting the end.
    #[test]
    fn a_cut_ends_a_request_whose_answer_has_not_begun_and_a_stream() {
        let pool = Pool::new(Duration::from_secs(60));
        std::thread::scope(|s| {
            let (mut theirs, exchange) = out(s, &pool, Flying::of("acme", &pool));
            request_on(&mut theirs);
            assert_eq!(state_of(&pool, "n1"), WAITING);
            assert_eq!(pool.cut("n1", "beta"), Cuts::default());
            assert_eq!(state_of(&pool, "n1"), WAITING);
            let cuts = pool.cut("n1", "acme");
            assert_eq!(
                cuts,
                Cuts {
                    waiting: 1,
                    streams: 0
                }
            );
            assert_eq!(ended(exchange.join().unwrap()), Ended::Cut { whole: true });
            theirs
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            assert_eq!(theirs.read(&mut [0u8; 16]).unwrap(), 0);
            assert_eq!(state_of(&pool, "n1"), CLOSED);
        });
        std::thread::scope(|s| {
            let (mut theirs, exchange) = out(s, &pool, Flying::of("acme", &pool));
            request_on(&mut theirs);
            let head = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n";
            theirs.write_all(head).unwrap();
            let Ok(answer) = exchange.join().unwrap() else {
                panic!("the stream's head was not read");
            };
            let mut stream = answer.into_stream(&pool).unwrap();
            assert_eq!(state_of(&pool, "n1"), STREAMING);
            theirs.write_all(b"event: seed\ndata: []\n\n").unwrap();
            // Each read handed over as it comes, its end as an empty one.
            let (got, came) = std::sync::mpsc::channel();
            s.spawn(move || {
                let mut chunk = [0u8; 1024];
                loop {
                    let n = stream.read(&mut chunk).unwrap_or(0);
                    let _ = got.send(chunk[..n].to_vec());
                    if n == 0 {
                        return;
                    }
                }
            });
            let mut seed = Vec::new();
            while !seed.ends_with(b"\n\n") {
                let chunk = came.recv_timeout(Duration::from_secs(10)).unwrap();
                assert!(
                    !chunk.is_empty(),
                    "the stream ended before its seed: {seed:?}"
                );
                seed.extend_from_slice(&chunk);
            }
            assert!(seed.starts_with(b"event: seed"), "{seed:?}");
            let cuts = pool.cut("n1", "acme");
            assert_eq!(
                cuts,
                Cuts {
                    waiting: 0,
                    streams: 1
                }
            );
            let end = came.recv_timeout(Duration::from_secs(10)).unwrap();
            assert!(end.is_empty(), "{end:?}");
        });
    }

    /// A cut of several tenants off a node at once -- a catch-up's of a
    /// whole node's failover -- ends their requests and no other's, and
    /// moves each of their buckets once: two in one bucket move it one
    /// count, a tenant named twice is cut once, and a tenant in another
    /// bucket out on the node is left to its answer, its bucket unmoved.
    #[test]
    fn a_cut_of_several_tenants_ends_theirs_alone_and_moves_each_bucket_once() {
        let pool = Pool::new(Duration::from_secs(60));
        let (beta, other) = (bucketed("acme", true), bucketed("acme", false));
        let seen = |t: &str| pool.moved(mark_of(t));
        let (acme_was, other_was) = (seen("acme"), seen(&other));
        std::thread::scope(|s| {
            let mut outs: Vec<_> = ["acme", beta.as_str(), other.as_str()]
                .into_iter()
                .map(|t| out(s, &pool, Flying::of(t, &pool)))
                .collect();
            for (theirs, _) in outs.iter_mut() {
                request_on(theirs);
            }
            let failed = [("n1", "acme"), ("n1", beta.as_str()), ("n1", "acme")];
            let cuts = pool.cut_all(&failed);
            assert_eq!(
                cuts,
                Cuts {
                    waiting: 2,
                    streams: 0
                }
            );
            assert_eq!(seen("acme"), acme_was + 1);
            assert_eq!(seen(&beta), acme_was + 1);
            assert_eq!(seen(&other), other_was);
            let (mut other_end, other_out) = outs.pop().unwrap();
            for (_, exchange) in outs {
                assert_eq!(ended(exchange.join().unwrap()), Ended::Cut { whole: true });
            }
            other_end
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
            let Ok(answer) = other_out.join().unwrap() else {
                panic!("the other tenant's request was cut");
            };
            assert_eq!(answer.status, 200);
        });
    }

    /// A cut of tenants off several nodes at once moves every bucket before
    /// it shuts any node's connections: a request cut off the first node,
    /// gone on with a count read then, would find it moved again by the
    /// second node's cut before its first byte -- `Ended::Moved`, and the
    /// cut lost (a copy's 503 under leases, passed on). Moved a node at a
    /// time, the first node's connections were shut with the bucket one
    /// short of where the cut left it.
    #[test]
    fn a_cut_off_several_nodes_moves_every_bucket_before_it_shuts_any() {
        let pool = Pool::new(Duration::from_secs(60));
        let (beta, other) = (bucketed("acme", true), bucketed("acme", false));
        let was = [pool.moved(mark_of("acme")), pool.moved(mark_of(&other))];
        let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let told = std::rc::Rc::clone(&seen);
        let (b, o) = (mark_of("acme"), mark_of(&other));
        SHUTTING.set(Some(Box::new(move |pool: &Pool, addr: &str| {
            let at = [pool.moved(b), pool.moved(o)];
            told.borrow_mut().push((addr.to_string(), at));
        })));
        let failed = [
            ("n1", "acme"),
            ("n2", beta.as_str()),
            ("n1", other.as_str()),
        ];
        let cuts = pool.cut_all(&failed);
        SHUTTING.set(None);
        assert_eq!(cuts, Cuts::default());
        let after = [was[0] + 1, was[1] + 1];
        assert_eq!(
            *seen.borrow(),
            [("n1".to_string(), after), ("n2".to_string(), after)]
        );
    }

    /// A stream whose tenant is failed over between its head and being
    /// handed over to be copied -- its answer begun, which the cut passes
    /// over, and not a stream yet -- ends as it is handed over: its tenant's
    /// bucket moved meanwhile. Not asked, it was copied for good, the node
    /// silent. A stream whose bucket did not move is handed over.
    #[test]
    fn a_stream_cut_between_its_head_and_its_copy_ends_as_it_is_handed_over() {
        let pool = Pool::new(Duration::from_secs(60));
        for cut in [true, false] {
            std::thread::scope(|s| {
                let (mut theirs, exchange) = out(s, &pool, Flying::of("acme", &pool));
                request_on(&mut theirs);
                let head = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n";
                theirs.write_all(head).unwrap();
                let Ok(answer) = exchange.join().unwrap() else {
                    panic!("the stream's head was not read");
                };
                if cut {
                    assert_eq!(state_of(&pool, "n1"), ANSWERING);
                    assert_eq!(pool.cut("n1", "acme"), Cuts::default());
                    let e = answer.into_stream(&pool).err().expect("it ends");
                    assert_eq!(ended_by(&e), Some(Ended::Cut { whole: true }));
                } else {
                    assert!(answer.into_stream(&pool).is_ok());
                }
            });
        }
    }

    /// A request whose answer's head is in is the node's: a cut of its
    /// tenant leaves it, and its body comes whole after the cut. And a
    /// request no failover knows (`Flying::NONE`) -- the router's own call
    /// to a node -- is never cut, whatever tenant is.
    #[test]
    fn a_cut_leaves_an_answer_begun_and_a_request_no_failover_knows() {
        let pool = Pool::new(Duration::from_secs(60));
        for flying in [Flying::of("acme", &pool), Flying::NONE] {
            std::thread::scope(|s| {
                let (mut theirs, exchange) = out(s, &pool, flying);
                request_on(&mut theirs);
                if flying.mark == 0 {
                    assert_eq!(pool.cut("n1", "acme"), Cuts::default());
                }
                theirs
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n")
                    .unwrap();
                let Ok(answer) = exchange.join().unwrap() else {
                    panic!("its head was not read");
                };
                assert_eq!(pool.cut("n1", "acme"), Cuts::default());
                theirs.write_all(b"hello").unwrap();
                assert_eq!(answer.read_body(&pool, false).unwrap(), b"hello");
            });
        }
    }

    /// A read replica's answer, read whole and kept for its head, hands the
    /// connection it came on back to the pool -- that very descriptor, open
    /// once the answer is dropped, a cut still finding its flight -- and
    /// the answer has no body left to read. A copy of the descriptor
    /// pooled in its place was a `dup` and a `close` more a read.
    #[test]
    fn an_answer_read_for_its_head_hands_its_own_connection_back() {
        use std::os::fd::AsRawFd;
        let pool = Pool::new(Duration::from_secs(60));
        std::thread::scope(|s| {
            let (mut theirs, exchange) = out(s, &pool, Flying::of("acme", &pool));
            request_on(&mut theirs);
            let fd = pool.idle.lock().unwrap()["n1"].flights[0].fd;
            theirs
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .unwrap();
            let Ok(mut answer) = exchange.join().unwrap() else {
                panic!("its head was not read");
            };
            assert_eq!(answer.read_body_kept(&pool, false).unwrap(), b"ok");
            assert_eq!(answer.status, 200);
            assert!(answer.reader.is_none(), "the answer keeps a connection");
            let again = answer.read_body_kept(&pool, false).unwrap_err();
            assert_eq!(again.to_string(), read_already().to_string());
            drop(answer);
            let idle = pool.idle.lock().unwrap();
            let kept = &idle["n1"];
            assert_eq!(kept.idle.len(), 1);
            assert_eq!(kept.idle[0].conn.stream.as_raw_fd(), fd);
            assert!(open(&kept.idle[0].conn.stream));
            assert_eq!(kept.flights.len(), 1);
            assert!(Arc::ptr_eq(&kept.flights[0], &kept.idle[0].conn.flight));
        });
    }

    /// A listener that answers no connect, as a machine gone does: its
    /// queue made short (`listen` again, which both systems take: 1, since
    /// macOS reads 0 as the most it allows) and filled, so that the next
    /// SYN is dropped. The connections that filled it, kept open.
    fn deaf() -> (TcpListener, Vec<TcpStream>) {
        use std::os::fd::AsRawFd;
        extern "C" {
            fn listen(fd: i32, backlog: i32) -> i32;
        }
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        // SAFETY: a listening socket's descriptor, open for the call.
        assert_eq!(unsafe { listen(l.as_raw_fd(), 1) }, 0);
        let at = l.local_addr().unwrap();
        let mut filled = Vec::new();
        while let Ok(s) = TcpStream::connect_timeout(&at, Duration::from_millis(500)) {
            filled.push(s);
            assert!(filled.len() < 64, "the queue never filled");
        }
        (l, filled)
    }

    /// A forward of `tenant` dialing `addr`, on a thread of its own: what a
    /// failover did to it, where one did.
    fn dialing(
        pool: &Arc<Pool>,
        addr: &str,
        tenant: &str,
    ) -> std::sync::mpsc::Receiver<Option<Ended>> {
        let flying = Flying::of(tenant, pool);
        let (told, dialed) = std::sync::mpsc::channel();
        let (p, a) = (Arc::clone(pool), addr.to_string());
        std::thread::spawn(move || {
            let none = Duration::ZERO;
            let sent = p.send_held(&a, "GET", "/t/acme/notes", &[], b"", none, None, flying);
            let _ = told.send(sent.err().and_then(|e| ended_by(&e)));
        });
        dialed
    }

    /// A forward dialing a node that answers no connect as a failover moves
    /// its tenant is given up on within a try of the connect, to be read
    /// from the directory again, where it waited out the pool's timeout: a
    /// minute here. And one whose connect is refused -- the node's process
    /// gone, its machine there -- as its tenant moves is read from the
    /// directory again too, where it was the client's 502.
    #[test]
    fn a_forward_dialing_a_node_that_answers_nothing_is_given_up_once_its_tenant_moves() {
        let (l, _filled) = deaf();
        let addr = l.local_addr().unwrap().to_string();
        let pool = Arc::new(Pool::new(Duration::from_secs(60)));
        let dialed = dialing(&pool, &addr, "acme");
        assert_eq!(pool.cut(&addr, "acme"), Cuts::default());
        let ended = dialed.recv_timeout(Duration::from_secs(20)).unwrap();
        assert_eq!(ended, Some(Ended::Moved));

        let gone = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = gone.local_addr().unwrap().to_string();
        drop(gone);
        let flying = Flying::of("acme", &pool);
        assert_eq!(pool.cut(&addr, "acme"), Cuts::default());
        let none = Duration::ZERO;
        let sent = pool.send_held(&addr, "GET", "/", &[], b"", none, None, flying);
        assert_eq!(sent.err().and_then(|e| ended_by(&e)), Some(Ended::Moved));
    }

    /// A forward whose tenant a failover moved after the directory was read
    /// and before its request went out writes nothing, and is read from the
    /// directory again; its connection, healthy, goes back to the pool. One
    /// straddling the failover of a tenant of another bucket goes out as it
    /// would have, on its pooled connection: one count for every tenant,
    /// any failover sent it to the directory again and closed the
    /// connection.
    #[test]
    fn a_request_whose_tenant_moved_before_it_went_out_writes_nothing() {
        let pool = Pool::new(Duration::from_secs(60));
        let flying = Flying::of("acme", &pool);
        assert_eq!(pool.cut("n1", &bucketed("acme", true)), Cuts::default());
        std::thread::scope(|s| {
            let (theirs, exchange) = out(s, &pool, flying);
            assert_eq!(ended(exchange.join().unwrap()), Ended::Moved);
            assert!(
                quiet(&theirs),
                "the node heard of a request that never went"
            );
            assert_eq!(state_of(&pool, "n1"), IDLE);
            let kept = pool.idle.lock().unwrap()["n1"].idle.len();
            assert_eq!(kept, 1, "its connection is the pool's again");
        });

        let pool = Pool::new(Duration::from_secs(60));
        let flying = Flying::of("acme", &pool);
        assert_eq!(pool.cut("n1", &bucketed("acme", false)), Cuts::default());
        std::thread::scope(|s| {
            let (mut theirs, exchange) = out(s, &pool, flying);
            request_on(&mut theirs);
            theirs
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
            let Ok(answer) = exchange.join().unwrap() else {
                panic!("it never went out");
            };
            assert_eq!(answer.status, 200);
        });
    }

    /// A node answering each connection as it takes it, before it reads a
    /// byte -- its accept loop at its thread limit -- and closing it at
    /// once: every write sent there is answered its 503, the node having
    /// run none. Head and body went in two `write`s, and on Linux the reset
    /// the closed socket sent the head back failed the body's, the 503
    /// unread: a send in 30 000 a broken pipe idle, 11 to 37% on three
    /// cores beside six busy loops. Either fix alone holds it (one
    /// `writev`; the answer read after a failed write), so it fails only
    /// with both taken out. A node now holds a refused connection and reads
    /// its request off (`fenec_http::accept::refuse`), and resets none, but
    /// one closes it at once still past the descriptors it holds them with,
    /// with no thread to hold them on, and a node from before in a rolling
    /// upgrade closes every one: the stand-in is such a node
    /// (`refuse_at_once`). With `refuse` it passed with both fixes out.
    #[test]
    fn a_node_refusing_a_connection_as_it_takes_it_is_answered_by_its_refusal() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            for c in l.incoming() {
                let Ok(c) = c else { continue };
                let answer = fenec_http::http::Response::retry_after(503, "no thread", 1);
                let why = fenec_http::accept::Refusal::NoThread;
                fenec_http::accept::refuse_at_once(c, answer, why);
            }
        });
        let pool = Pool::new(Duration::from_secs(10));
        let body = br#"{"query":"put notes {title: \"t\"}"}"#;
        for i in 0..2_000 {
            match pool.send(&addr, "POST", "/t/acme/query", &[], body) {
                Ok(a) => assert!(a.unserved(), "send {i}: {}", a.status),
                Err(e) => panic!("send {i}: {e}"),
            }
        }
    }

    /// A body longer than the sockets hold, sent to a node that answers
    /// once it has its first bytes and closes with the rest unread: the
    /// write meets the reset part way, and the answer that came before it
    /// is the answer -- the node never read the request whole, so it ran
    /// nothing. Taken for the write's failure, it was a 502.
    #[test]
    fn an_answer_in_before_the_body_went_out_whole_is_the_answer() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let node = std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let mut first = [0u8; 1];
            c.read_exact(&mut first).unwrap();
            c.write_all(b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                .unwrap();
        });
        let pool = Pool::new(Duration::from_secs(30));
        let body = vec![b'x'; 64 << 20];
        let a = pool
            .send(&addr, "POST", "/t/acme/query", &[], &body)
            .unwrap_or_else(|e| panic!("the node's answer, not {e}"));
        assert_eq!(a.status, 413);
        assert_eq!(a.read_body(&pool, false).unwrap(), b"{}");
        node.join().unwrap();
    }

    /// The same with a 200: no fenec node answers a request it has not read
    /// whole but to refuse it, so a success in before the body went out is
    /// a peer claiming what it cannot have done -- the write stays unsent,
    /// and the answer is not passed on.
    #[test]
    fn a_success_in_before_the_body_went_out_whole_is_no_answer() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let node = std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let mut first = [0u8; 1];
            c.read_exact(&mut first).unwrap();
            c.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                .unwrap();
        });
        let pool = Pool::new(Duration::from_secs(30));
        let body = vec![b'x'; 64 << 20];
        let sent = pool.send(&addr, "POST", "/t/acme/query", &[], &body);
        assert!(
            sent.is_err(),
            "taken for an answer: {:?}",
            sent.map(|a| a.status)
        );
        node.join().unwrap();
    }
}
