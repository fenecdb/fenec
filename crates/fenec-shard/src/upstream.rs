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

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Response header ceiling, same as the server side's.
const MAX_HEAD: usize = fenec_http::http::MAX_HEADER_BYTES;

/// Idle connections kept per node. Beyond it a returned connection is
/// closed: the pool bounds descriptors, the router's thread count bounds
/// concurrency.
const MAX_IDLE: usize = 32;

pub struct Pool {
    idle: Mutex<HashMap<String, Vec<Idle>>>,
    timeout: Duration,
}

/// A pooled connection and the read timeout it was left with, so that a
/// request waiting as long sets none: the two `setsockopt`s every exchange
/// made took 0.67 us, more than the look (`open`) made in their place.
struct Idle {
    stream: TcpStream,
    read_timeout: Duration,
    /// Set at the connect, and only by a read replica's read after
    /// ([`Pool::send_within`]): the pool's timeout otherwise, so a request
    /// sets it on no connection a replica's read has not used.
    write_timeout: Duration,
}

pub struct Answer {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    reader: BufReader<TcpStream>,
    addr: String,
    reusable: bool,
    read_timeout: Duration,
    write_timeout: Duration,
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
        }
    }

    fn take(&self, addr: &str) -> Option<Idle> {
        self.idle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(addr)
            .and_then(|v| v.pop())
    }

    fn put(&self, addr: &str, s: Idle) {
        let mut idle = self.idle.lock().unwrap_or_else(|e| e.into_inner());
        let list = idle.entry(addr.to_string()).or_default();
        if list.len() < MAX_IDLE {
            list.push(s);
        }
    }

    fn connect(&self, addr: &str, within: Duration, write: Duration) -> io::Result<TcpStream> {
        dial(addr, within, write)
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
        self.send_held(addr, method, target, headers, body, Duration::ZERO, None)
    }

    /// [`Pool::send`] of a request the node may hold for `held` before it
    /// answers (`Fenec-Wait`): the answer is waited for that much longer.
    /// Cut at the timeout alone, a claim held longer was answered 502 here
    /// while the node went on to take a job for it, leased to no one.
    /// `client`, the socket of the request's own client, is watched while
    /// the head is waited for ([`answered_or_gone`]): its end is passed on
    /// to the node, and its reset lets go of the node's connection, the
    /// failure [`client_gone`]. `None` waits as an unheld request does,
    /// with not a system call more.
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
        match self.send_staged(addr, method, target, headers, body, times, replay, client) {
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
        match self.send_staged(addr, method, target, headers, body, times, replay, None) {
            Ok(a) => Ok(a),
            Err(Failure(Stage::Unsent | Stage::Unread, e)) => Err(Near::Unreached(e)),
            Err(Failure(Stage::Unanswered | Stage::Other | Stage::Abandoned, e)) => {
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
        self.send_staged(addr, method, target, headers, body, times, replay, None)
            .map_err(|Failure(_, e)| e)
    }

    /// [`Self::send_timed`], its failure with how far it got, `client`
    /// watched while the head is waited for ([`Pool::send_held`]).
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
        while let Some(idle) = self.take(addr) {
            if !open(&idle.stream) {
                continue;
            }
            let set = (Some(idle.read_timeout), idle.write_timeout);
            let want = (read_timeout, write_timeout);
            match self.exchange(addr, idle.stream, set, (head, body), want, client) {
                Ok(a) => return Ok(a),
                Err(Failure(Stage::Unsent, _)) => break,
                Err(Failure(Stage::Unanswered, _)) if replay.allows(method, headers) => break,
                Err(f) => return Err(f),
            }
        }
        let s = self
            .connect(addr, connect_within, write_timeout)
            .map_err(|e| Failure(Stage::Unsent, e))?;
        let set = (None, write_timeout);
        let want = (read_timeout, write_timeout);
        self.exchange(addr, s, set, (head, body), want, client)
    }

    /// One request, its head and body, and the head of its answer over
    /// `s`, whose read and write timeouts are `set` -- the read's where it
    /// is known -- each read of the answer bounded by `want.0` and each
    /// write by `want.1`. Each is set only where it is not already, a
    /// system call a request. `client` is watched until the answer begins
    /// ([`answered_or_gone`]).
    fn exchange(
        &self,
        addr: &str,
        s: TcpStream,
        set: (Option<Duration>, Duration),
        (head, body): (&[u8], &[u8]),
        want: (Duration, Duration),
        client: Option<&TcpStream>,
    ) -> Result<Answer, Failure> {
        let (read_timeout, write_timeout) = want;
        if set.0 != Some(read_timeout) {
            s.set_read_timeout(Some(read_timeout))
                .map_err(|e| Failure(Stage::Unsent, e))?;
        }
        if set.1 != write_timeout {
            s.set_write_timeout(Some(write_timeout))
                .map_err(|e| Failure(Stage::Unsent, e))?;
        }
        let mut w = &s;
        // A `write` that fails took none of its bytes, so the request's
        // last byte never went out: an empty body writes nothing, and the
        // head's last byte is then the last.
        w.write_all(head)
            .and_then(|_| w.write_all(body))
            .map_err(|e| {
                // Timed out, the node is there and not reading: sent again,
                // it would wait as long.
                let stage = if timed_out(&e) {
                    Stage::Unread
                } else {
                    Stage::Unsent
                };
                Failure(stage, e)
            })?;
        // The client's end, passed on as the node reads one from a client
        // of its own: its held claim sees it before its next run, and its
        // answer comes back as any. Shut for writing, the connection is
        // not kept for another request.
        let mut client_ended = false;
        if let Some(client) = client {
            if answered_or_gone(&s, client, read_timeout)? == Watched::Ended {
                let _ = s.shutdown(std::net::Shutdown::Write);
                client_ended = true;
            }
        }

        let mut reader = BufReader::new(s);
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => {
                return Err(Failure(
                    Stage::Unanswered,
                    io::ErrorKind::UnexpectedEof.into(),
                ))
            }
            Ok(_) => {}
            // The connection's end, as each system reports it. Not any
            // error with nothing read: a line that is not UTF-8 is left
            // out of `line`, and that is an answer.
            Err(e) if line.is_empty() && ended(&e) => return Err(Failure(Stage::Unanswered, e)),
            Err(e) => return Err(Failure(Stage::Other, e)),
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
        let reusable = !client_ended
            && !headers.iter().any(|(k, v)| {
                k.eq_ignore_ascii_case("connection") && v.eq_ignore_ascii_case("close")
            });
        Ok(Answer {
            status,
            headers,
            reader,
            addr: addr.to_string(),
            reusable,
            read_timeout,
            write_timeout,
        })
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
fn replayable(method: &str, headers: &[(&str, &str)]) -> bool {
    matches!(method, "GET" | "HEAD" | "OPTIONS")
        || headers.iter().any(|(k, v)| {
            k.eq_ignore_ascii_case("idempotency-key")
                || (k.eq_ignore_ascii_case(fenec_http::READ_HEADER) && fenec_http::marks_read(v))
        })
}

/// A connection to `addr`, each address it resolves to tried within
/// `within`, its writes bounded by `write`.
fn dial(addr: &str, within: Duration, write: Duration) -> io::Result<TcpStream> {
    use std::net::ToSocketAddrs;
    let mut last = io::Error::new(io::ErrorKind::NotFound, format!("{addr} does not resolve"));
    for a in addr.to_socket_addrs()? {
        match TcpStream::connect_timeout(&a, within) {
            Ok(s) => {
                let _ = s.set_nodelay(true);
                // Once a connection: the same for every request but a read
                // replica's, which sets its own and is tracked with the
                // connection (`Idle::write_timeout`).
                s.set_write_timeout(Some(write))?;
                return Ok(s);
            }
            Err(e) => last = e,
        }
    }
    Err(last)
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
    #[repr(C)]
    struct PollFd {
        fd: i32,
        events: i16,
        revents: i16,
    }
    // `nfds_t`: an `unsigned int` on Apple's systems and in Android's
    // bionic, an `unsigned long` in glibc and musl.
    #[cfg(any(target_vendor = "apple", target_os = "android"))]
    type Nfds = std::ffi::c_uint;
    #[cfg(all(target_os = "linux", not(target_vendor = "apple")))]
    type Nfds = std::ffi::c_ulong;
    extern "C" {
        fn poll(fds: *mut PollFd, n: Nfds, timeout: i32) -> i32;
    }
    // The same on both: `<sys/poll.h>`'s and `<asm-generic/poll.h>`'s.
    const POLLIN: i16 = 0x1;
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

impl Answer {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// `None` means the body runs to the close: a stream.
    pub fn content_length(&self) -> Option<usize> {
        self.header("content-length")?.parse().ok()
    }

    /// [`Answer::read_body`], the answer kept for its head: the connection
    /// handed back to the pool as a copy of its descriptor, and this
    /// answer's reader, read to its end, dropped with it. What reads a
    /// read replica's whole answer before a byte of it goes to the client,
    /// so that one broken off goes home instead.
    pub fn read_body_kept(&mut self, pool: &Pool, head_only: bool) -> io::Result<Vec<u8>> {
        let Some(len) = self.content_length() else {
            return Err(bad("a stream is read as it comes"));
        };
        let len = if head_only { 0 } else { len };
        let body = body_of(&mut self.reader, len, |_| Ok(()))?;
        if self.reusable && self.reader.buffer().is_empty() {
            if let Ok(stream) = self.reader.get_ref().try_clone() {
                let idle = Idle {
                    stream,
                    read_timeout: self.read_timeout,
                    write_timeout: self.write_timeout,
                };
                pool.put(&self.addr, idle);
            }
        }
        self.reusable = false;
        Ok(body)
    }

    /// Reads a `Content-Length` body and hands the connection back to the
    /// pool when the node keeps it open.
    pub fn read_body(mut self, pool: &Pool, head_only: bool) -> io::Result<Vec<u8>> {
        let Some(len) = self.content_length() else {
            let mut body = Vec::new();
            self.reader.read_to_end(&mut body)?;
            return Ok(body);
        };
        let len = if head_only { 0 } else { len };
        let body = body_of(&mut self.reader, len, |_| Ok(()))?;
        // Bytes left in the buffer would be the start of an answer nobody
        // asked for; such a connection is not reused.
        if self.reusable && self.reader.buffer().is_empty() {
            let idle = Idle {
                stream: self.reader.into_inner(),
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
        let timeout = &mut self.read_timeout;
        let body = body_of(&mut self.reader, len, |reader| {
            if reader.buffer().is_empty() {
                let left = left_until(deadline)?.min(*timeout);
                reader.get_ref().set_read_timeout(Some(left))?;
                *timeout = left;
            }
            Ok(())
        })?;
        if self.reusable && self.reader.buffer().is_empty() {
            let idle = Idle {
                stream: self.reader.into_inner(),
                read_timeout: self.read_timeout,
                write_timeout: self.write_timeout,
            };
            pool.put(&self.addr, idle);
        }
        Ok(body)
    }

    /// The rest of the answer as a byte stream, with no read timeout: an SSE
    /// stream is silent until something changes (the node's keep-alive
    /// line bounds the silence).
    pub fn into_stream(self) -> io::Result<BufReader<TcpStream>> {
        self.reader.get_ref().set_read_timeout(None)?;
        Ok(self.reader)
    }
}

/// Whether the node at `addr` answers `GET /_health` within `within`, on a
/// connection of its own, made for the question and closed after, as a
/// client new to the node reaches it. Any status is an answer: a node
/// answers it on the connection's own thread before any token, lock or
/// routing, so one that answers at all takes connections and runs them
/// ([`crate::readers::HEALTH_WAIT`]). Asked on a thread of its own, so the
/// name's resolution, which `within` does not bound, holds up no request.
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
    reader: &mut BufReader<TcpStream>,
    len: usize,
    mut before: impl FnMut(&BufReader<TcpStream>) -> io::Result<()>,
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
mod tests {
    use super::*;
    use std::net::TcpListener;

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
        match pool.exchange("node", s, both, (head, b""), want, None) {
            Ok(_) => "answered",
            Err(Failure(Stage::Unsent, _)) => "unsent",
            Err(Failure(Stage::Unanswered, _)) => "unanswered",
            Err(Failure(Stage::Unread | Stage::Other | Stage::Abandoned, e)) => panic!("{e}"),
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

    /// An answer to one request, its head and `body`, from a node's end
    /// that reads the request first and closes after: the head read.
    fn answered(pool: &Pool, length: usize, body: Vec<u8>) -> Answer {
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
            let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {length}\r\n\r\n");
            let _ = theirs.write_all(head.as_bytes());
            let _ = theirs.write_all(&body);
        });
        let head = b"GET /t/acme/notes HTTP/1.1\r\nContent-Length: 0\r\n\r\n";
        let (set, want) = ((None, pool.timeout), (pool.timeout, pool.timeout));
        match pool.exchange("node", ours, set, (head, b""), want, None) {
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
}
