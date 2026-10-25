//! A thread for each connection, and the answer to one that gets none:
//! what `fenec-server`'s accept loop and `fenec-shard`'s share.
//!
//! Every connection is a thread. Past `--max-connections` one is refused
//! 503 on the accepting thread, and where the system makes no thread --
//! macOS's 2 048 a process, a cgroup's `pids.max`, systemd's `TasksMax=`,
//! `ulimit -u` -- `thread::Builder::spawn` fails. The stream went with the
//! closure it could not run, closed with nothing said: its client, whose
//! request was on its way, read `Connection reset by peer`, and a router
//! forwarding a write to such a node answered it 502, "may have run" --
//! written whole, the connection ended before a byte of an answer, which
//! is what a node that ran it and went down looks like. Such a connection
//! is now answered as the ceiling answers one ([`refuse`]): 503 with
//! `Retry-After`, `{"error", "retry_after"}` and [`UNSERVED_HEADER`],
//! which a router passes on and a client comes back after, both knowing
//! the request never ran.
//!
//! A refused connection is not closed with the answer: closed, its
//! client's request, on its way still, met a socket that was gone and was
//! answered a reset, and a client writing it in two parts (Python's
//! `http.client`, head then body) had the second fail `EPIPE` and never
//! read the 503 waiting for it -- 300 such clients against a ceiling of
//! 100 read 2 968 503s and 23 026 broken pipes of 26 016 refusals on
//! Linux, 3 732 and 36 361 of 40 194 on macOS. Its write side is shut
//! after the answer and the stream held by one thread of the process
//! ([`linger`]), which reads off what arrives until the client ends it,
//! so nothing comes back a reset.

use crate::http::Response;
use std::cell::Cell;
use std::io::{self, Read};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::Builder;
use std::time::{Duration, Instant};

/// What a connection with no thread is told, the node's and the router's.
pub const NO_THREAD: &str = "no thread for the connection: the server is at its thread limit";

/// The header of an answer written as its connection was taken, before a
/// byte of the request was read, the [`Refusal`]'s reason its value: the
/// request ran nothing. No standard header says so -- `Retry-After` says
/// when, not whether -- and with it the global entry and a router relaying
/// to a region's routers send any request on to the next router, as one
/// never run, and note this one as having answered nothing: final, as a
/// router's own 503 is, a router at its thread limit kept every request
/// from the standby beside it. A router drops it from a node's answer it
/// forwards, its own connection having been served.
pub const UNSERVED_HEADER: &str = "Fenec-Unserved";

/// Why a connection was answered on the accepting thread and ended,
/// unserved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Past `--max-connections`.
    Ceiling = 0,
    /// No thread could be made for it.
    NoThread = 1,
}

/// [`Refusal`]s as `reason` labels them, in its order.
pub const REFUSALS: [&str; 2] = ["ceiling", "no_thread"];

#[allow(clippy::declare_interior_mutable_const)]
const ZERO: AtomicU64 = AtomicU64::new(0);

/// Connections refused, by [`Refusal`]: the process's, the node's and the
/// router's alike (`fenec_connections_refused_total`,
/// `fenec_router_connections_refused_total`). The accepting thread is one,
/// so no shard.
static REFUSED: [AtomicU64; 2] = [ZERO; 2];

/// When a connection with no thread was last said in the log: a moment of
/// the monotonic clock, which a wall clock set back does not hold silent.
static SAID: Mutex<Option<Instant>> = Mutex::new(None);

/// How many connections were refused for `why`.
pub fn refused(why: Refusal) -> u64 {
    REFUSED[why as usize].load(Ordering::Relaxed)
}

/// The most of a refused client's request read off on the accepting
/// thread before the close, where no thread holds the connection.
const DRAIN: usize = 64 << 10;

/// The stream a spawn's closure holds, given back to the accepting thread
/// where the closure never runs: `Builder::spawn` drops a closure it made
/// no thread for on the thread that called it, so the stream goes into
/// [`UNSERVED`] there rather than closing, and [`spawn`] hands it back. A
/// closure that runs takes the stream out first, and its drop, on the
/// connection's thread, finds nothing. Safe where a raw descriptor taken
/// out and made a stream again would not be, and nothing on the way: no
/// `try_clone` (a `dup` a connection), no allocation.
struct Handed(Option<TcpStream>);

thread_local! {
    static UNSERVED: Cell<Option<TcpStream>> = const { Cell::new(None) };
}

impl Drop for Handed {
    fn drop(&mut self) {
        if let Some(s) = self.0.take() {
            UNSERVED.with(|u| u.set(Some(s)));
        }
    }
}

/// A connection no thread was made for: why, and the stream, for the
/// caller to answer ([`no_thread`]) -- `None` only were a closure dropped
/// elsewhere than on the thread that spawned it, which no std does.
pub struct Unserved {
    pub stream: Option<TcpStream>,
    pub error: io::Error,
}

/// `serve(stream)` on a thread `builder` makes, or the stream back where
/// none can be made.
#[inline]
pub fn spawn(
    builder: Builder,
    stream: TcpStream,
    fail: &FailSpawns,
    serve: impl FnOnce(TcpStream) + Send + 'static,
) -> Result<(), Unserved> {
    let handed = Handed(Some(stream));
    let spawned = fail.apply(builder).spawn(move || {
        let mut handed = handed;
        if let Some(stream) = handed.0.take() {
            serve(stream);
        }
    });
    match spawned {
        Ok(_) => Ok(()),
        Err(error) => Err(Unserved {
            stream: UNSERVED.with(Cell::take),
            error,
        }),
    }
}

/// Answers `stream` with `answer` on the accepting thread, marked
/// [`UNSERVED_HEADER`], ends it, and counts it under `why`.
///
/// The socket is made non-blocking first: the accepting thread is every
/// connection's, and a client that reads nothing must not hold it. The
/// answer, a few hundred bytes into a new socket's empty send buffer, goes
/// out whole in one write all the same; where it would not, it is let go
/// of. The request is not waited for -- the answer does not hang on it --
/// but the socket is not closed after the answer either: its write side is
/// shut, so the client reads the answer and its end, and the stream goes
/// to the [`linger`]ing thread, which reads off the request as it arrives
/// and closes the socket once the client has ended it. Closed at once, a
/// socket whose request had not arrived answered it with a reset, and one
/// whose request had arrived unread was reset as it closed, on macOS and on
/// Linux alike: a client writing its body after its head had the write fail
/// `EPIPE`, the 503 unread, and macOS handed some a reset before the answer
/// it had received. Where no thread holds it -- none could be made, an
/// answer did not go out whole, or [`most_held`] are held already -- what
/// has arrived is read off here, up to 64 KiB ([`DRAIN`]), and the socket
/// closed, as before ([`refuse_at_once`]).
pub fn refuse(stream: TcpStream, answer: Response, why: Refusal) {
    refuse_saying(stream, answer, why, true, || {});
}

/// [`refuse`] where the connection is not held: what has arrived read off
/// on this thread, up to [`DRAIN`], and the socket closed -- as every
/// refusal was before it was held, as one is past [`most_held`] or with no
/// lingering thread, and as a node from before answers a router in a
/// rolling upgrade. A request still on its way meets the reset. A test's:
/// the stand-in for such a node, whose refusals the router's one `writev`
/// and its read after a failed write must still read as answers, and the
/// way `accept`'s unit test reaches the read off on this thread.
#[doc(hidden)]
pub fn refuse_at_once(stream: TcpStream, answer: Response, why: Refusal) {
    refuse_saying(stream, answer, why, false, || {});
}

/// [`refuse`], held where `hold` and a thread takes it, `say` run while the
/// answer's request id is the thread's, so a line it writes carries it
/// ([`crate::request_id::log`]).
fn refuse_saying(
    mut stream: TcpStream,
    answer: Response,
    why: Refusal,
    hold: bool,
    say: impl FnOnce(),
) {
    REFUSED[why as usize].fetch_add(1, Ordering::Relaxed);
    let answer = answer.header(UNSERVED_HEADER, REFUSALS[why as usize]);
    crate::request_id::begin(None);
    if stream.set_nonblocking(true).is_ok() {
        // An answer that did not go out whole is no answer to hold the
        // connection open for.
        let answered = answer.write(&mut stream, false, false).is_ok()
            && stream.shutdown(Shutdown::Write).is_ok();
        let unheld = if answered && hold {
            linger(stream)
        } else {
            Err(stream)
        };
        if let Err(stream) = unheld {
            let mut buf = [0u8; 8 << 10];
            let mut left = DRAIN;
            read_off(&stream, &mut buf, &mut left);
        }
    }
    say();
    crate::request_id::end();
}

/// The queue of connections the system completes for a listener before it
/// is taken: std's `TcpListener::bind` asks 128.
const BACKLOG: i32 = 1024;

/// Asks the system for a queue of [`BACKLOG`] connections on `listener`,
/// listening on it again, which changes the queue's length alone (Linux and
/// macOS alike): each system holds it to its own ceiling --
/// `net.core.somaxconn`, 4 096 on Linux since 5.4, `kern.ipc.somaxconn`,
/// 128 on macOS. Past the queue Linux drops a connection's handshake
/// (`ListenOverflows`), its client trying again after a second.
pub fn widen(listener: &std::net::TcpListener) {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        extern "C" {
            fn listen(socket: std::ffi::c_int, backlog: std::ffi::c_int) -> std::ffi::c_int;
        }
        // SAFETY: the listener's own descriptor, open while it is borrowed;
        // a failure leaves std's queue, and is nothing to stop for.
        unsafe { listen(listener.as_raw_fd(), BACKLOG) };
    }
    #[cfg(not(unix))]
    let _ = listener;
}

/// How long a refused connection is held after its answer for its client
/// to end it, its request read off as it arrives: a client that reads its
/// answer ends it at once, so this bounds only one that does not.
const LINGER: Duration = Duration::from_secs(2);

/// The most refused connections handed over or held at once, a descriptor
/// each, where the process may open 2 048 or more ([`most_held`]): past
/// it, one is closed as it is answered, what has arrived read off first.
const LINGERING: usize = 512;

/// [`LINGERING`], or a quarter of the descriptors the process may open
/// where that is fewer -- macOS gives a process 256 unless it is raised, a
/// shell on Linux 1 024 -- so the held refusals leave the rest to the
/// connections it serves and its files. Read once.
fn most_held() -> usize {
    static MOST: OnceLock<usize> = OnceLock::new();
    *MOST.get_or_init(|| open_files().map_or(LINGERING, |n| LINGERING.min(n / 4)))
}

/// The process's soft limit on open descriptors (`RLIMIT_NOFILE`), where
/// this build knows the call's numbers: the crate takes no `libc`.
#[cfg(any(
    target_vendor = "apple",
    all(
        any(target_os = "linux", target_os = "android"),
        any(
            target_arch = "x86_64",
            target_arch = "aarch64",
            target_arch = "x86",
            target_arch = "arm",
            target_arch = "riscv64",
            target_arch = "powerpc64",
            target_arch = "s390x",
            target_arch = "loongarch64"
        )
    )
))]
fn open_files() -> Option<usize> {
    #[cfg(target_vendor = "apple")]
    type Limit = u64;
    #[cfg(not(target_vendor = "apple"))]
    type Limit = std::ffi::c_ulong;
    #[cfg(target_vendor = "apple")]
    const RLIMIT_NOFILE: std::ffi::c_int = 8;
    #[cfg(not(target_vendor = "apple"))]
    const RLIMIT_NOFILE: std::ffi::c_int = 7;
    #[repr(C)]
    struct Rlimit {
        cur: Limit,
        max: Limit,
    }
    extern "C" {
        fn getrlimit(resource: std::ffi::c_int, rlim: *mut Rlimit) -> std::ffi::c_int;
    }
    let mut r = Rlimit { cur: 0, max: 0 };
    // SAFETY: `r` is the struct the call fills, alive across it.
    let asked = unsafe { getrlimit(RLIMIT_NOFILE, &mut r) };
    (asked == 0).then(|| usize::try_from(r.cur).unwrap_or(usize::MAX))
}

#[cfg(not(any(
    target_vendor = "apple",
    all(
        any(target_os = "linux", target_os = "android"),
        any(
            target_arch = "x86_64",
            target_arch = "aarch64",
            target_arch = "x86",
            target_arch = "arm",
            target_arch = "riscv64",
            target_arch = "powerpc64",
            target_arch = "s390x",
            target_arch = "loongarch64"
        )
    )
)))]
fn open_files() -> Option<usize> {
    None
}

/// The most of a refused client's bytes read off while it is held: past
/// them, a body still coming meets the reset, after the answer.
const LINGER_DRAIN: usize = 1 << 20;

/// The first pause before a held connection is looked at again, doubled
/// each look to [`LONGEST_LOOK`]: a client ends it within a look or two.
const FIRST_LOOK: Duration = Duration::from_millis(1);
const LONGEST_LOOK: Duration = Duration::from_millis(64);

/// Streams handed to the lingering thread, not yet taken, and its wake.
static HANDED: Mutex<Vec<TcpStream>> = Mutex::new(Vec::new());
static WAKE: Condvar = Condvar::new();
/// Refused connections handed over or held: at most [`most_held`].
static HOLDING: AtomicUsize = AtomicUsize::new(0);
/// Whether the lingering thread runs; `STARTING` makes one at a time.
static LINGERER: AtomicBool = AtomicBool::new(false);
static STARTING: Mutex<()> = Mutex::new(());

/// Makes the process's lingering thread where none runs yet: each accept
/// loop asks before its first connection, so a process at its thread limit
/// has it already -- a new accept loop calls it, and [`widen`], before its
/// first accept. False where it could not be made.
pub fn ready() -> bool {
    if LINGERER.load(Ordering::Acquire) {
        return true;
    }
    let _one = STARTING.lock().unwrap_or_else(|e| e.into_inner());
    if LINGERER.load(Ordering::Acquire) {
        return true;
    }
    most_held();
    // Its stack holds a loop and no buffer: the buffer is the heap's.
    let made = Builder::new()
        .name("fenec-linger".into())
        .stack_size(64 << 10)
        .spawn(hold);
    LINGERER.store(made.is_ok(), Ordering::Release);
    made.is_ok()
}

/// Hands `stream`, answered and shut for writing, to the lingering thread,
/// or gives it back where none runs or [`most_held`] are held.
fn linger(stream: TcpStream) -> Result<(), TcpStream> {
    if !ready() {
        return Err(stream);
    }
    if HOLDING.fetch_add(1, Ordering::Relaxed) >= most_held() {
        HOLDING.fetch_sub(1, Ordering::Relaxed);
        return Err(stream);
    }
    HANDED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(stream);
    WAKE.notify_one();
    Ok(())
}

/// A refused connection held open: until when, when it is looked at next,
/// the pause after that, and how much more of it is read off.
struct Held {
    stream: TcpStream,
    until: Instant,
    next: Instant,
    pause: Duration,
    left: usize,
}

/// The lingering thread: takes what is handed over, looks at each held
/// connection as it falls due, reading off what arrived, and closes one
/// whose client ended it, reset it, or sent past [`LINGER_DRAIN`], or that
/// was held [`LINGER`]. No `poll` (std has none): each is read again after
/// a pause doubling from [`FIRST_LOOK`], so a client that never ends one
/// costs a read every [`LONGEST_LOOK`], and the thread sleeps while it
/// holds none.
fn hold() {
    let mut held: Vec<Held> = Vec::new();
    let mut taken: Vec<TcpStream> = Vec::new();
    let mut buf = vec![0u8; 64 << 10];
    loop {
        {
            let mut handed = HANDED.lock().unwrap_or_else(|e| e.into_inner());
            while handed.is_empty() {
                let now = Instant::now();
                match held.iter().map(|h| h.next).min() {
                    None => handed = WAKE.wait(handed).unwrap_or_else(|e| e.into_inner()),
                    Some(next) if next > now => {
                        handed = WAKE
                            .wait_timeout(handed, next - now)
                            .unwrap_or_else(|e| e.into_inner())
                            .0;
                    }
                    Some(_) => break,
                }
            }
            std::mem::swap(&mut *handed, &mut taken);
        }
        let now = Instant::now();
        held.extend(taken.drain(..).map(|stream| Held {
            stream,
            until: now + LINGER,
            next: now,
            pause: FIRST_LOOK,
            left: LINGER_DRAIN,
        }));
        let before = held.len();
        held.retain_mut(|h| {
            if h.next > now {
                return true;
            }
            if !read_off(&h.stream, &mut buf, &mut h.left) || now >= h.until {
                return false;
            }
            h.next = now + h.pause;
            h.pause = (h.pause * 2).min(LONGEST_LOOK);
            true
        });
        HOLDING.fetch_sub(before - held.len(), Ordering::Relaxed);
    }
}

/// Reads off what has arrived on the non-blocking `stream`, `left` bytes at
/// most: whether it is still open, its client neither ended nor reset it
/// and `left` not spent.
fn read_off(mut stream: &TcpStream, buf: &mut [u8], left: &mut usize) -> bool {
    while *left > 0 {
        let n = buf.len().min(*left);
        match stream.read(&mut buf[..n]) {
            Ok(0) => return false,
            Ok(n) => *left -= n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return true,
            Err(_) => return false,
        }
    }
    false
}

/// Whether a connection with no thread is said in the log now: a minute
/// after the last one said at least.
fn due() -> bool {
    let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    if said.is_some_and(|at| now.duration_since(at) < Duration::from_secs(60)) {
        return false;
    }
    *said = Some(now);
    true
}

/// [`refuse`]s the stream no thread could be made for, and says so in the
/// log a minute apart at most, the line carrying the answer's request id:
/// a process at its thread limit refuses every connection it cannot serve,
/// and a line each would be a line a request. `counted` names the family
/// that counts each.
#[cold]
#[inline(never)]
pub fn no_thread(unserved: Unserved, answer: Response, counted: &str) {
    let Unserved { stream, error } = unserved;
    debug_assert!(
        stream.is_some(),
        "a spawn that failed dropped its closure on another thread"
    );
    let say = due();
    let Some(stream) = stream else {
        // Its closure dropped where no stream comes back from, it closed
        // unanswered, as every such connection did before: counted, and
        // said, all the same.
        REFUSED[Refusal::NoThread as usize].fetch_add(1, Ordering::Relaxed);
        if say {
            crate::log!(
                "no thread for a connection: {error}; it was closed unanswered, its stream \
                 dropped with the closure that could not run. The process is at its thread \
                 limit (said a minute apart at most; {counted} counts each)"
            );
        }
        return;
    };
    // Its address only where it is said: a system call each otherwise.
    let from = say.then(|| {
        stream
            .peer_addr()
            .map_or_else(|_| "?".into(), |p| p.to_string())
    });
    refuse_saying(stream, answer, Refusal::NoThread, true, || {
        if let Some(from) = from {
            crate::log!(
                "no thread for a connection from {from}: {error}; answered 503 with Retry-After. \
                 The process is at its thread limit (a cgroup's pids.max, systemd's TasksMax=, \
                 ulimit -u, macOS's 2 048 a process): raise it, or lower --max-connections \
                 below it (said a minute apart at most; {counted} counts each)"
            );
        }
    });
}

/// Spawns a test has fail ([`crate::Server::fail_spawns`]): the next `n`
/// connections' threads are asked for a stack no address space holds
/// (`NO_STACK`), so the system refuses each as it refuses a thread past
/// its limit -- `pthread_create`'s EAGAIN -- and [`spawn`] fails where it
/// would, its closure dropped as at the limit. Reaching the real limit
/// would fail every other test's threads in the process. One relaxed load
/// a connection where none is asked for.
#[doc(hidden)]
#[derive(Clone, Default)]
pub struct FailSpawns(Arc<AtomicUsize>);

/// A stack past every address space a system maps -- 2 EiB, where 64-bit
/// systems map 2^57 bytes at most -- so its mapping fails. 2^46 bytes was
/// mapped, on macOS and in Docker's Linux; 2^50 to 2^62 were each refused
/// EAGAIN on macOS, glibc and musl.
const NO_STACK: usize = 1 << (usize::BITS - 3);

impl FailSpawns {
    /// The next `n` spawns fail.
    pub fn next(&self, n: usize) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }

    /// `builder`, made to fail where a spawn is still to fail.
    #[inline]
    fn apply(&self, builder: Builder) -> Builder {
        let mut n = self.0.load(Ordering::Relaxed);
        while n > 0 {
            match self
                .0
                .compare_exchange_weak(n, n - 1, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return builder.stack_size(NO_STACK),
                Err(now) => n = now,
            }
        }
        builder
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    /// A spawn made to fail gives its stream back, open, and one that runs
    /// is served on its thread.
    #[test]
    fn a_spawn_that_fails_gives_the_stream_back() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let fail = FailSpawns::default();
        fail.next(1);
        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let (s, _) = l.accept().unwrap();
        let Err(back) = spawn(Builder::new(), s, &fail, |_| panic!("never run")) else {
            panic!("the spawn made to fail ran");
        };
        assert_eq!(
            back.error.kind(),
            io::ErrorKind::WouldBlock,
            "{}",
            back.error
        );
        let mut s = back.stream.expect("the stream given back");
        s.write_all(b"x").unwrap();
        let mut one = [0u8; 1];
        c.read_exact(&mut one).unwrap();
        assert_eq!(&one, b"x");

        let (s, _) = {
            let _c2 = TcpStream::connect(("127.0.0.1", port)).unwrap();
            l.accept().unwrap()
        };
        let (tx, rx) = std::sync::mpsc::channel();
        assert!(spawn(Builder::new(), s, &fail, move |_| tx.send(()).unwrap()).is_ok());
        rx.recv().unwrap();
    }

    /// Ends `c` and waits for it to be over -- its end acknowledged, or
    /// reset; either way it has no peer -- then the error a reset left on
    /// it, if one came.
    fn over(c: &TcpStream) -> Option<String> {
        // Refused where a reset came first: the reset is what is asked.
        let _ = c.shutdown(Shutdown::Write);
        let by = Instant::now() + Duration::from_secs(30);
        while c.peer_addr().is_ok() {
            assert!(Instant::now() < by, "the connection never ended");
            std::thread::sleep(Duration::from_millis(1));
        }
        match c.take_error() {
            Ok(None) => None,
            Ok(Some(e)) => Some(format!("after the end: {e}")),
            Err(e) => Some(format!("its error: {e}")),
        }
    }

    /// A refusal is the answer and an end, not a reset: the request that
    /// had arrived is read off before the close -- held, by the lingering
    /// thread; closed at once ([`refuse_at_once`], past [`most_held`] or
    /// with no lingering thread), here, up to [`DRAIN`]. Closed with the
    /// request unread, the socket was reset: macOS's client read
    /// `ECONNRESET` after the answer, and Linux's, which reads the end the
    /// shut writing sent before it, was left `EPIPE`.
    #[test]
    fn a_refusal_ends_the_connection_after_its_answer() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        type Refuse = fn(TcpStream, Response, Refusal);
        let ways: [(&str, Refuse); 2] = [("held", refuse), ("at once", refuse_at_once)];
        for (way, refusing) in ways {
            let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
            c.set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .unwrap();
            c.write_all(b"GET / HTTP/1.1\r\nHost: t\r\n\r\n").unwrap();
            let (s, _) = l.accept().unwrap();
            // The request in before the refusal, as a client's is: the
            // peek waits for it, the one segment it went in.
            let mut one = [0u8; 1];
            assert_eq!(s.peek(&mut one).unwrap(), 1);
            let answer = Response::retry_after(503, NO_THREAD, 1);
            refusing(s, answer, Refusal::NoThread);
            let mut out = String::new();
            c.read_to_string(&mut out)
                .unwrap_or_else(|e| panic!("{way}: the answer, then an end, not {e}"));
            assert!(out.starts_with("HTTP/1.1 503 "), "{out}");
            assert!(out.contains("\r\nRetry-After: 1\r\n"), "{out}");
            assert!(out.contains("\r\nFenec-Unserved: no_thread\r\n"), "{out}");
            assert!(out.ends_with(r#""retry_after":1}"#), "{out}");
            assert_eq!(over(&c), None, "{way}");
        }
    }

    /// The descriptors the process may open, as the call asks them, are
    /// the ones a shell it starts reads: the call's numbers are this
    /// system's, so [`most_held`] is a quarter of the real limit.
    #[cfg(any(target_vendor = "apple", target_os = "linux"))]
    #[test]
    fn the_descriptor_limit_is_the_one_a_shell_reads() {
        let shell = std::process::Command::new("sh")
            .args(["-c", "ulimit -n"])
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&shell.stdout).trim().to_string();
        let asked = open_files().expect("the limit asked");
        match said.as_str() {
            "unlimited" => assert!(asked >= 1 << 30, "{asked}"),
            n => assert_eq!(Some(asked), n.parse().ok(), "a shell read {n}"),
        }
        assert_eq!(most_held(), LINGERING.min(asked / 4));
    }

    /// A body larger than a socket's send buffer, macOS's 128 KiB and
    /// Linux's first 16 KiB alike, and smaller than what a held connection
    /// is read off: written to a socket that is gone, the write waits for
    /// room the gone socket never makes, and the reset ends it -- every
    /// time, where a body that fit went out whole before the reset came in
    /// on macOS more often than not.
    const BODY: usize = 512 << 10;

    /// Reads `c`'s answer and its end -- where the refusal closed the
    /// socket, it had closed it by then -- then writes a request, head and
    /// [`BODY`] apart as Python's `http.client` writes them, ends the
    /// connection and waits for it to be over: the error a reset left on
    /// it, if one came.
    fn written_after_the_end(mut c: TcpStream) -> Result<(), String> {
        let mut out = String::new();
        c.read_to_string(&mut out)
            .map_err(|e| format!("the answer: {e}"))?;
        assert!(out.starts_with("HTTP/1.1 503 "), "{out}");
        let head = format!("POST /query HTTP/1.1\r\nHost: t\r\nContent-Length: {BODY}\r\n\r\n");
        c.write_all(head.as_bytes())
            .map_err(|e| format!("the head: {e}"))?;
        c.write_all(&vec![b' '; BODY])
            .map_err(|e| format!("the body: {e}"))?;
        match over(&c) {
            None => Ok(()),
            Some(e) => Err(e),
        }
    }

    /// A request that arrives after the refusal's answer and end is read
    /// off, and the connection ends with no reset. Closed with its answer,
    /// the socket answered the head with a reset and the body's write
    /// failed, `EPIPE` or `ECONNRESET`, on Linux and macOS each time -- a
    /// client writing as the answer came had the 503 unread.
    #[test]
    fn a_request_after_the_refusal_is_read_off_not_reset() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        for i in 0..3 {
            let c = TcpStream::connect(("127.0.0.1", port)).unwrap();
            c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let (s, _) = l.accept().unwrap();
            refuse(
                s,
                Response::retry_after(503, NO_THREAD, 1),
                Refusal::NoThread,
            );
            assert_eq!(written_after_the_end(c), Ok(()), "connection {i}");
        }
    }
}
