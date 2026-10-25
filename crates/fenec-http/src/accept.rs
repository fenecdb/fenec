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

use crate::http::Response;
use std::cell::Cell;
use std::io::{self, Read};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
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

/// Why a connection was answered on the accepting thread and closed.
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

/// The most of a refused client's request read off before the close.
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
/// [`UNSERVED_HEADER`], closes it, and counts it under `why`.
///
/// The socket is made non-blocking first: the accepting thread is every
/// connection's, and a client that reads nothing must not hold it. The
/// answer, a few hundred bytes into a new socket's empty send buffer, goes
/// out whole in one write all the same; where it would not, it is let go
/// of. The request is not read first -- what has not arrived cannot be
/// waited for here, and the answer does not hang on it -- but what has
/// arrived is read off after the answer, up to 64 KiB (`DRAIN`),
/// non-blocking too: closed with its request unread, a socket is reset
/// rather than ended, on macOS and on Linux alike (probed), and a client
/// reading to the end read `Connection reset by peer` after the answer. A
/// request that arrives after the close, or a body past what had arrived,
/// still meets the reset, which a client reads after the answer, since both
/// systems hand a socket's received bytes over before its error.
pub fn refuse(stream: TcpStream, answer: Response, why: Refusal) {
    refuse_saying(stream, answer, why, || {});
}

/// [`refuse`], `say` run while the answer's request id is the thread's, so
/// a line it writes carries it ([`crate::request_id::log`]).
fn refuse_saying(mut stream: TcpStream, answer: Response, why: Refusal, say: impl FnOnce()) {
    REFUSED[why as usize].fetch_add(1, Ordering::Relaxed);
    let answer = answer.header(UNSERVED_HEADER, REFUSALS[why as usize]);
    crate::request_id::begin(None);
    if stream.set_nonblocking(true).is_ok() {
        let _ = answer.write(&mut stream, false, false);
        let mut buf = [0u8; 8 << 10];
        let mut left = DRAIN;
        while left > 0 {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => left = left.saturating_sub(n),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    }
    say();
    crate::request_id::end();
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
    refuse_saying(stream, answer, Refusal::NoThread, || {
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

    /// A refusal is the answer and an end, not a reset: the request that
    /// had arrived is read off before the close.
    #[test]
    fn a_refusal_ends_the_connection_after_its_answer() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        c.set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        c.write_all(b"GET / HTTP/1.1\r\nHost: t\r\n\r\n").unwrap();
        let (s, _) = l.accept().unwrap();
        // The request in before the refusal, as a client's is: the peek
        // waits for it, the one segment it went in.
        let mut one = [0u8; 1];
        assert_eq!(s.peek(&mut one).unwrap(), 1);
        let answer = Response::retry_after(503, NO_THREAD, 1);
        refuse(s, answer, Refusal::NoThread);
        let mut out = String::new();
        c.read_to_string(&mut out)
            .expect("the answer, then an end: no reset");
        assert!(out.starts_with("HTTP/1.1 503 "), "{out}");
        assert!(out.contains("\r\nRetry-After: 1\r\n"), "{out}");
        assert!(out.contains("\r\nFenec-Unserved: no_thread\r\n"), "{out}");
        assert!(out.ends_with(r#""retry_after":1}"#), "{out}");
    }
}
