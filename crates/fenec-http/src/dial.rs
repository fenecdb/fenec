//! A name looked up, and a connection made, within a bound the caller sets
//! -- and walked away from past it.
//!
//! `getaddrinfo` blocks for as long as the system's resolver takes, and no
//! timeout reaches it: std's `to_socket_addrs` is that call, and
//! `TcpStream::connect_timeout` bounds the connect alone. A name whose
//! resolver is slow to refuse it held each caller for as long as the
//! resolver took -- musl's 5 s, glibc's 5 s a try and a server -- and the
//! slow refusal is what a container's name meets once the container is
//! gone: Docker's resolver no longer knows it and forwards it upstream,
//! which may answer late or never. A router's lease round waited on every
//! grant, so the grant to such a node held up every other node's renewal:
//! in V5's drill on GitHub's runner the region's healthy nodes went 18 s
//! without a write and were failed over themselves, and the dead node's
//! tenants wrote again 30 s after it went.
//!
//! So a name is looked up on a thread of its own ([`resolve`]), waited for
//! no longer than the caller's bound: past it the caller has a timeout, and
//! the lookup runs to its end with nobody waiting, its answer dropped. A
//! caller asking a name whose lookup began less than [`JOIN_WITHIN`] ago,
//! or that has [`MAX_IN_FLIGHT`] lookups out already, waits on the newest
//! of them rather than start another: a dead node's name asked by every
//! request to its tenants would otherwise leave a thread a request
//! lingering for the resolver's 5 s, and a name that answers in a
//! millisecond is asked once for a burst of connects. One that comes back
//! after hanging is asked afresh within [`JOIN_WITHIN`] of the newest
//! lookup still out, not at that lookup's end.
//!
//! An address written out (`10.0.0.5:8081`, `[::1]:8081`) is read where it
//! stands, with no thread and no lookup, as `to_socket_addrs` reads one: a
//! router whose nodes are recorded by address pays nothing for any of this.
//! Nothing is cached: the addresses a lookup gave go to the connects that
//! asked, and the next connect asks again, as before. Kept past their
//! lookup, a node's old address sends a router's grant to whatever machine
//! took it after the node left -- Docker hands a stopped container's
//! address to the next one it starts.
//!
//! [`connect_unless`] is a connect a caller can walk away from at a word:
//! the whole dial, lookup and connect, on a thread of its own, the caller
//! looking at its stop flag while it waits. A replica's follower dials its
//! primary so: halted -- a promotion halts it before the fork -- in the
//! middle of reconnecting to a dead primary, it waited out that connect's
//! 5 s, and the failover with it. A follower reconnecting holds a dial
//! thread beside its own, and one walked away from runs on to its bound
//! (the lookup's 5 s, then the connect's): a `--dir` standby whose N
//! tenants follow a primary gone holds N of them at most, each for that
//! long, and the follower waits out its backoff before the next.

use std::io;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// A lookup of a name begun less than this long ago is waited on by the
/// next caller asking it, not begun again: a burst of connects asks once.
pub const JOIN_WITHIN: Duration = Duration::from_millis(500);

/// The most lookups of one name out at once: past it a caller waits on the
/// newest. A resolver that never answers costs this many threads a name,
/// each until the system's resolver gives up on it.
pub const MAX_IN_FLIGHT: usize = 8;

/// How often [`connect_unless`] looks at its stop flag while it waits.
const STOP_LOOK: Duration = Duration::from_millis(10);

/// What a lookup came to: `io::Error` is no `Clone`, and each caller
/// waiting on the lookup takes one of its own.
type Found = Result<Vec<SocketAddr>, (io::ErrorKind, String)>;

struct Lookup {
    addr: String,
    began: Instant,
    found: Mutex<Option<Found>>,
    came: Condvar,
}

/// The lookups out now, by name: a handful at most, each there only while
/// its thread runs.
static OUT: Mutex<Vec<Arc<Lookup>>> = Mutex::new(Vec::new());

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The addresses `addr` (`host:port`) stands for, looked up within
/// `within`: a timeout past it, whatever the resolver is doing (the
/// module's comment).
pub fn resolve(addr: &str, within: Duration) -> io::Result<Vec<SocketAddr>> {
    if let Ok(a) = addr.parse::<SocketAddr>() {
        return Ok(vec![a]);
    }
    let timed_out = || {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!("`{addr}` did not resolve within {within:?}"),
        )
    };
    // No time at all: no lookup begun for nobody to wait on.
    if within.is_zero() {
        return Err(timed_out());
    }
    let now = Instant::now();
    // A bound past what an `Instant` holds is no bound.
    let deadline = now.checked_add(within);
    let lookup = joined(addr, now)?;
    let mut found = lock(&lookup.found);
    loop {
        if let Some(f) = &*found {
            return f.clone().map_err(|(kind, why)| io::Error::new(kind, why));
        }
        found = match deadline {
            None => lookup.came.wait(found).unwrap_or_else(|e| e.into_inner()),
            Some(deadline) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(timed_out());
                }
                lookup
                    .came
                    .wait_timeout(found, left)
                    .unwrap_or_else(|e| e.into_inner())
                    .0
            }
        };
    }
}

/// The lookup of `addr` a caller asking `at` waits on: the newest one out
/// where it began less than [`JOIN_WITHIN`] before, or where as many are
/// out as may be; a new one otherwise.
fn joined(addr: &str, at: Instant) -> io::Result<Arc<Lookup>> {
    let mut out = lock(&OUT);
    let same = out.iter().filter(|l| l.addr == addr);
    let count = same.clone().count();
    if let Some(newest) = same.max_by_key(|l| l.began) {
        if count >= MAX_IN_FLIGHT || at.saturating_duration_since(newest.began) < JOIN_WITHIN {
            return Ok(Arc::clone(newest));
        }
    }
    let lookup = Arc::new(Lookup {
        addr: addr.to_string(),
        began: Instant::now(),
        found: Mutex::new(None),
        came: Condvar::new(),
    });
    let theirs = Arc::clone(&lookup);
    std::thread::Builder::new()
        .name("fenec-resolve".into())
        .spawn(move || {
            let found = system_lookup(&theirs.addr).map_err(|e| (e.kind(), e.to_string()));
            *lock(&theirs.found) = Some(found);
            theirs.came.notify_all();
            lock(&OUT).retain(|l| !Arc::ptr_eq(l, &theirs));
        })?;
    out.push(Arc::clone(&lookup));
    Ok(lookup)
}

/// The system's resolver, or a test's stand-in for a name it holds.
fn system_lookup(addr: &str) -> io::Result<Vec<SocketAddr>> {
    if STOOD_IN.load(Ordering::Relaxed) {
        let f = lock(&STAND_INS)
            .iter()
            .find(|(name, _)| name == addr)
            .map(|(_, f)| Arc::clone(f));
        if let Some(f) = f {
            return f();
        }
    }
    Ok(addr.to_socket_addrs()?.collect())
}

type StandIn = Arc<dyn Fn() -> io::Result<Vec<SocketAddr>> + Send + Sync>;

static STAND_INS: Mutex<Vec<(String, StandIn)>> = Mutex::new(Vec::new());
/// Whether any name is stood in for: one load a lookup where none is.
static STOOD_IN: AtomicBool = AtomicBool::new(false);

/// Has `addr` looked up by `f` in place of the system's resolver: what a
/// test makes a slow or silent resolver of, for one name of its own, the
/// rest of the process's lookups untouched. A second stand-in for the name
/// replaces the first.
#[doc(hidden)]
pub fn stand_in(addr: &str, f: impl Fn() -> io::Result<Vec<SocketAddr>> + Send + Sync + 'static) {
    let mut all = lock(&STAND_INS);
    all.retain(|(name, _)| name != addr);
    all.push((addr.to_string(), Arc::new(f)));
    STOOD_IN.store(true, Ordering::Relaxed);
}

/// A connection to `addr`: its name looked up within `within`
/// ([`resolve`]), then each address it stands for tried within `within`,
/// as `TcpStream::connect_timeout` tries one.
pub fn connect(addr: &str, within: Duration) -> io::Result<TcpStream> {
    let mut last = io::Error::new(io::ErrorKind::NotFound, format!("{addr} does not resolve"));
    for a in resolve(addr, within)? {
        match TcpStream::connect_timeout(&a, within) {
            Ok(s) => {
                let _ = s.set_nodelay(true);
                return Ok(s);
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// [`connect`], given up on as soon as `stop` is set: the dial runs on a
/// thread of its own and is left to end there, its connection, if it makes
/// one, closed unused. An `Interrupted` error when stopped. Its answer
/// comes back through a slot and a condvar, as a lookup's does: an `mpsc`
/// channel was 13 to 14 KB more of each binary's code.
pub fn connect_unless(addr: &str, within: Duration, stop: &AtomicBool) -> io::Result<TcpStream> {
    type Slot = (Mutex<Option<io::Result<TcpStream>>>, Condvar);
    let slot: Arc<Slot> = Arc::new((Mutex::new(None), Condvar::new()));
    let (theirs, to) = (Arc::clone(&slot), addr.to_string());
    std::thread::Builder::new()
        .name("fenec-dial".into())
        .spawn(move || {
            let dialed = connect(&to, within);
            *lock(&theirs.0) = Some(dialed);
            theirs.1.notify_all();
        })?;
    let mut dialed = lock(&slot.0);
    loop {
        if stop.load(Ordering::SeqCst) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                format!("stopped while connecting to {addr}"),
            ));
        }
        if let Some(d) = dialed.take() {
            return d;
        }
        dialed = slot
            .1
            .wait_timeout(dialed, STOP_LOOK)
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// A stand-in for `name` that answers `to` once its gate is opened.
    fn gated(name: &str, to: SocketAddr) -> Arc<(Mutex<bool>, Condvar)> {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let g = Arc::clone(&gate);
        stand_in(name, move || {
            let (open, opened) = &*g;
            let mut open = lock(open);
            while !*open {
                open = opened.wait(open).unwrap_or_else(|e| e.into_inner());
            }
            Ok(vec![to])
        });
        gate
    }

    fn open(gate: &(Mutex<bool>, Condvar)) {
        *lock(&gate.0) = true;
        gate.1.notify_all();
    }

    fn out_for(name: &str) -> usize {
        lock(&OUT).iter().filter(|l| l.addr == name).count()
    }

    /// A name whose lookup hangs is a timeout at the caller's bound, and a
    /// caller asking it meanwhile shares the lookup; once it answers, a
    /// caller has the address and connects to it.
    #[test]
    fn a_lookup_that_hangs_is_a_timeout_and_is_shared() {
        let name = "hangs.dial.test:1";
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let to = l.local_addr().unwrap();
        let gate = gated(name, to);
        let asked = Instant::now();
        let e = resolve(name, Duration::from_millis(50)).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut, "{e}");
        // Bounded from below only: the caller waited its bound.
        assert!(asked.elapsed() >= Duration::from_millis(50));
        // Asked at the moment the first was, the lookup out is joined, not
        // begun again -- asked by the clock, a loaded machine could put the
        // second ask past JOIN_WITHIN.
        let shared = joined(name, asked).unwrap();
        assert_eq!(out_for(name), 1);
        assert!(lock(&shared.found).is_none());
        let waiting = std::thread::spawn(move || resolve(name, Duration::from_secs(30)));
        open(&gate);
        assert_eq!(waiting.join().unwrap().unwrap(), vec![to]);
        assert!(connect(name, Duration::from_secs(5)).is_ok());
    }

    /// Past [`MAX_IN_FLIGHT`] lookups of a name out at once, a caller waits
    /// on the newest rather than begin another: the threads a name that
    /// never answers costs are bounded.
    #[test]
    fn a_name_that_never_answers_costs_a_bounded_number_of_lookups() {
        let name = "silent.dial.test:1";
        let gate = gated(name, "127.0.0.1:9".parse().unwrap());
        // Each ask a JOIN_WITHIN after the one before, as callers asking
        // that far apart find the newest lookup too old to join.
        let start = Instant::now();
        for i in 1..=(MAX_IN_FLIGHT as u32 * 3) {
            joined(name, start + JOIN_WITHIN * i).unwrap();
        }
        assert_eq!(out_for(name), MAX_IN_FLIGHT);
        open(&gate);
    }

    /// A connect walked away from returns as soon as its stop flag is set,
    /// the lookup it was waiting on hanging still.
    #[test]
    fn a_connect_is_given_up_at_the_word() {
        let name = "stopped.dial.test:1";
        let gate = gated(name, "127.0.0.1:9".parse().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let s = Arc::clone(&stop);
        let dialing = std::thread::spawn(move || connect_unless(name, Duration::from_secs(60), &s));
        while out_for(name) == 0 {
            std::thread::yield_now();
        }
        stop.store(true, Ordering::SeqCst);
        let e = dialing.join().unwrap().unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::Interrupted, "{e}");
        open(&gate);
    }

    /// Asked within no time at all, a name is a timeout with no lookup
    /// begun for nobody to wait on; asked within more than an `Instant`
    /// holds, it is waited for as long as it takes, not a panic.
    #[test]
    fn no_bound_and_a_bound_past_any_instant_are_both_taken() {
        let name = "bounds.dial.test:1";
        let to: SocketAddr = "127.0.0.1:9".parse().unwrap();
        stand_in(name, move || Ok(vec![to]));
        let e = resolve(name, Duration::ZERO).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut, "{e}");
        assert_eq!(out_for(name), 0);
        assert_eq!(resolve(name, Duration::MAX).unwrap(), vec![to]);
    }

    /// An address written out is read where it stands: asked within no
    /// time at all, a lookup would be a timeout.
    #[test]
    fn an_address_written_out_is_not_looked_up() {
        for a in ["127.0.0.1:8081", "[::1]:8081"] {
            assert_eq!(
                resolve(a, Duration::ZERO).unwrap(),
                vec![a.parse::<SocketAddr>().unwrap()]
            );
        }
    }
}
