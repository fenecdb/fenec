//! The id of the request a thread serves: `X-Request-Id`.
//!
//! Every request is given one as it is read -- the client's own when it
//! sent a usable one, a new one otherwise -- and it goes out on the answer
//! and into every line the request writes: the audit log's, the slow
//! statements', an error on stderr. A router hands its id on to the node,
//! so one request through `fenec-shard` has one id from end to end, and a
//! client, a proxy's access log and both servers' logs can be joined on it.
//!
//! A connection is a thread and serves one request at a time, so the id is
//! a thread-local, as who the audit log names is: set where the request is
//! read, read where an answer or a line is written, with no signature in
//! between having to carry it. The buffer is the thread's, so setting it
//! allocates nothing past a connection's first request.
//!
//! A made id is 16 hex characters: 64 bits of a bijective mix
//! (SplitMix64's finalizer) of a key read from the system once a process,
//! the thread's number and its count of requests -- distinct within a
//! process while it has made fewer than 2^24 threads, as unguessable as the
//! key, and 15 ns to make (12 to keep a client's), where reading
//! `/dev/urandom` for each was a system call a request.

use std::cell::{Cell, RefCell};
use std::fmt;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// The header the id comes in and goes out in.
pub const HEADER: &str = "X-Request-Id";

/// The longest id taken from a client; a longer one is replaced. A log
/// line carries it whole, so it is bounded as a line's statement is.
pub const MAX_LEN: usize = 128;

static KEY: OnceLock<u64> = OnceLock::new();
static THREADS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static ID: RefCell<String> = const { RefCell::new(String::new()) };
    /// The thread's number in the high 24 bits, its requests below.
    static NEXT: Cell<u64> = Cell::new(THREADS.fetch_add(1, Ordering::Relaxed) << 40);
}

fn key() -> u64 {
    *KEY.get_or_init(|| {
        let b = crate::crypto::random_bytes(8);
        u64::from_le_bytes(b[..8].try_into().unwrap_or([0; 8]))
    })
}

/// SplitMix64's finalizer: a bijection, so distinct inputs give distinct ids.
fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Whether a client's id is taken as it is: 1 to [`MAX_LEN`] bytes, each
/// printable ASCII. Nothing else can go into a header or a log line
/// unescaped, and a header line that held a CR or an LF would split the
/// answer.
pub fn valid(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_LEN && id.bytes().all(|b| (0x20..0x7f).contains(&b))
}

/// This thread's id from now on: `given` when it is [`valid`], a new one
/// otherwise.
pub fn begin(given: Option<&str>) {
    ID.with(|id| {
        let mut id = id.borrow_mut();
        id.clear();
        match given {
            Some(g) if valid(g) => id.push_str(g),
            _ => made(&mut id),
        }
    });
}

/// [`begin`] with the request's own header.
pub fn begin_request(req: &crate::http::Request) {
    begin(req.header(HEADER));
}

/// A new id onto `out`.
fn made(out: &mut String) {
    let n = NEXT.with(|n| {
        let v = n.get();
        n.set(v.wrapping_add(1));
        v
    });
    let mut v = mix(n ^ key());
    let mut hex = [0u8; 16];
    for b in hex.iter_mut().rev() {
        *b = b"0123456789abcdef"[(v & 15) as usize];
        v >>= 4;
    }
    // Hex digits alone: ASCII.
    out.push_str(std::str::from_utf8(&hex).unwrap_or_default());
}

/// The thread serves no request from now on: a line it writes carries no
/// id. A thread that never began one has none.
pub fn end() {
    ID.with(|id| id.borrow_mut().clear());
}

/// `f` with this thread's id, empty when it serves no request.
pub fn with<R>(f: impl FnOnce(&str) -> R) -> R {
    ID.with(|id| f(&id.borrow()))
}

/// This thread's id, owned: for a router that sends it on beside the
/// headers it borrows.
pub fn current() -> String {
    with(str::to_string)
}

/// A line on stderr (`log!`), with the id of the request it was written
/// for, if any, at its end: `... request_id=<id>`. Never panics, as
/// `eprintln!` would on a closed stderr.
pub fn log(args: fmt::Arguments) {
    let mut line = Vec::with_capacity(128);
    let _ = line.write_fmt(args);
    with(|id| {
        if !id.is_empty() {
            line.extend_from_slice(b" request_id=");
            line.extend_from_slice(id.as_bytes());
        }
    });
    line.push(b'\n');
    let _ = std::io::stderr().write_all(&line);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_usable_id_is_kept_and_any_other_replaced() {
        begin(Some("abc-123 x"));
        assert_eq!(current(), "abc-123 x");
        for bad in ["", "a\r\nb", "tab\there", "é", &"x".repeat(MAX_LEN + 1)] {
            begin(Some(bad));
            let id = current();
            assert_eq!(id.len(), 16, "{bad:?}: {id}");
            assert!(id.bytes().all(|b| b.is_ascii_hexdigit()), "{id}");
        }
        begin(Some(&"y".repeat(MAX_LEN)));
        assert_eq!(current().len(), MAX_LEN);
        end();
        assert_eq!(current(), "");
    }

    #[test]
    fn made_ids_differ_on_a_thread_and_across_threads() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..10_000 {
            begin(None);
            assert!(seen.insert(current()));
        }
        let other = std::thread::spawn(|| {
            (0..10_000)
                .map(|_| {
                    begin(None);
                    current()
                })
                .collect::<Vec<_>>()
        })
        .join()
        .unwrap();
        for id in other {
            assert!(seen.insert(id));
        }
    }
}
